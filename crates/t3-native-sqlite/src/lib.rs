//! t3-native-sqlite: N-API addon that runs the t3 serve SQLite worker pool
//! (4 readers + 1 writer + 1 WAL checkpoint) on dedicated Rust OS threads
//! instead of six `node:worker_threads` V8 isolates.
//!
//! Drop-in surface for `nodeSqliteClient.ts` `makeWorkerConnection` +
//! `startCheckpointWorker`: `NativeSqliteWorker`, `NativeCheckpointWorker`,
//! `sqliteVersion`.

use std::collections::HashMap;
use std::ptr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::*;
use napi::sys;
use napi::{Env, Error, JsDeferred, Status};
use napi_derive::napi;

/// Lifetime-free JS value wrapper for `#[napi]` returns and deferred resolves.
pub struct NapiValue(sys::napi_value);

impl ToNapiValue for NapiValue {
  unsafe fn to_napi_value(_env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
    Ok(val.0)
  }
}
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Statement};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

type Resolve<T> = Box<dyn FnOnce(Env) -> Result<T> + Send>;

#[derive(Debug)]
struct SqlErr {
  operation: &'static str,
  message: String,
  errcode: Option<i32>,
  errstr: Option<String>,
  code: &'static str,
}

impl SqlErr {
  fn sqlite(operation: &'static str, err: rusqlite::Error) -> Self {
    let (errcode, errstr) = match err.sqlite_error() {
      Some(e) => (Some(e.extended_code), Some(e.to_string())),
      None => (None, None),
    };
    SqlErr {
      operation,
      message: err.to_string(),
      errcode,
      errstr,
      code: "ERR_SQLITE_ERROR",
    }
  }

  fn plain(operation: &'static str, message: impl Into<String>) -> Self {
    SqlErr {
      operation,
      message: message.into(),
      errcode: None,
      errstr: None,
      code: "ERR_SQLITE_ERROR",
    }
  }

  fn not_wal(mode: &str) -> Self {
    SqlErr {
      operation: "open",
      message: format!("database uses journal_mode {mode}, not wal"),
      errcode: None,
      errstr: None,
      code: "ERR_SQLITE_NOT_WAL",
    }
  }
}

/// Build a JS `Error` carrying `operation`, `code`, `errcode`, `errstr`, then
/// retain it inside a `napi::Error` so the promise rejects with this exact value.
fn sql_err_to_napi(env: &Env, err: SqlErr) -> Error {
  match build_error_object(env, &err) {
    Ok(raw) => Error::from(unsafe { Unknown::from_raw_unchecked(env.raw(), raw) }),
    Err(e) => e,
  }
}

fn set_prop(
  env: sys::napi_env,
  obj: sys::napi_value,
  name: &str,
  value: sys::napi_value,
) -> Result<()> {
  let key = unsafe { make_string(env, name) }?;
  check_status!(unsafe { sys::napi_set_property(env, obj, key, value) })
}

fn build_error_object(env: &Env, err: &SqlErr) -> Result<sys::napi_value> {
  let raw_env = env.raw();
  unsafe {
    let message = make_string(raw_env, &err.message)?;
    let mut object = ptr::null_mut();
    check_status!(
      sys::napi_create_error(raw_env, ptr::null_mut(), message, &mut object),
      "napi_create_error failed"
    )?;
    let op = make_string(raw_env, err.operation)?;
    set_prop(raw_env, object, "operation", op)?;
    let code = make_string(raw_env, err.code)?;
    set_prop(raw_env, object, "code", code)?;
    let errcode = match err.errcode {
      Some(code) => {
        let mut v = ptr::null_mut();
        check_status!(
          sys::napi_create_int32(raw_env, code, &mut v),
          "napi_create_int32 failed"
        )?;
        v
      }
      None => {
        let mut v = ptr::null_mut();
        check_status!(
          sys::napi_get_undefined(raw_env, &mut v),
          "napi_get_undefined failed"
        )?;
        v
      }
    };
    set_prop(raw_env, object, "errcode", errcode)?;
    let errstr = match &err.errstr {
      Some(str_) => make_string(raw_env, str_)?,
      None => {
        let mut v = ptr::null_mut();
        check_status!(
          sys::napi_get_undefined(raw_env, &mut v),
          "napi_get_undefined failed"
        )?;
        v
      }
    };
    set_prop(raw_env, object, "errstr", errstr)?;
    Ok(object)
  }
}

fn settle<T: ToNapiValue + Send + 'static>(
  deferred: JsDeferred<T, Resolve<T>>,
  outcome: std::result::Result<T, SqlErr>,
) {
  match outcome {
    Ok(value) => deferred.resolve(Box::new(move |_env| Ok(value))),
    Err(err) => deferred.resolve(Box::new(move |env| Err(sql_err_to_napi(&env, err)))),
  }
}

// ---------------------------------------------------------------------------
// JS value -> SqlValue decoding (node:sqlite binding parity)
// ---------------------------------------------------------------------------

enum BindParams {
  Positional(Vec<SqlValue>),
  Named(HashMap<String, SqlValue>),
}

fn parameter_error(env: &Env, message: &str) -> Error {
  sql_err_to_napi(env, SqlErr::plain("execute", message))
}

fn napi_type(env: sys::napi_env, value: sys::napi_value) -> Result<sys::napi_valuetype> {
  let mut ty = 0;
  check_status!(unsafe { sys::napi_typeof(env, value, &mut ty) })?;
  Ok(ty)
}

fn is_array(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut array = false;
  check_status!(unsafe { sys::napi_is_array(env, value, &mut array) })?;
  Ok(array)
}

unsafe fn is_array_buffer(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut flag = false;
  check_status!(sys::napi_is_arraybuffer(env, value, &mut flag))?;
  Ok(flag)
}

/// Only copy from regular ArrayBuffers. A SharedArrayBuffer can be modified
/// concurrently by JS in another isolate while Rust is reading it.
unsafe fn extract_bytes(env: &Env, value: sys::napi_value) -> Result<Option<Vec<u8>>> {
  let raw = env.raw();
  let mut data = ptr::null_mut();
  let mut len = 0;
  if is_array_buffer(raw, value)? {
    check_status!(sys::napi_get_arraybuffer_info(
      raw, value, &mut data, &mut len
    ))?;
  } else {
    let mut buffer = ptr::null_mut();
    let mut flag = false;
    check_status!(sys::napi_is_typedarray(raw, value, &mut flag))?;
    if flag {
      let mut ty = 0;
      check_status!(sys::napi_get_typedarray_info(
        raw,
        value,
        &mut ty,
        &mut len,
        &mut data,
        &mut buffer,
        ptr::null_mut(),
      ))?;
      // N-API appended Float16Array as kind 11 after the BigInt array types.
      const FLOAT16_ARRAY: i32 = 11;
      let element_size = match ty {
        sys::TypedarrayType::int8_array
        | sys::TypedarrayType::uint8_array
        | sys::TypedarrayType::uint8_clamped_array => 1,
        sys::TypedarrayType::int16_array | sys::TypedarrayType::uint16_array | FLOAT16_ARRAY => 2,
        sys::TypedarrayType::int32_array
        | sys::TypedarrayType::uint32_array
        | sys::TypedarrayType::float32_array => 4,
        sys::TypedarrayType::float64_array
        | sys::TypedarrayType::bigint64_array
        | sys::TypedarrayType::biguint64_array => 8,
        _ => return Err(parameter_error(env, "unsupported typed array parameter")),
      };
      len = len
        .checked_mul(element_size)
        .ok_or_else(|| parameter_error(env, "BLOB parameter is too large"))?;
    } else {
      check_status!(sys::napi_is_dataview(raw, value, &mut flag))?;
      if !flag {
        return Ok(None);
      }
      check_status!(sys::napi_get_dataview_info(
        raw,
        value,
        &mut len,
        &mut data,
        &mut buffer,
        ptr::null_mut(),
      ))?;
    }
    if buffer.is_null() || !is_array_buffer(raw, buffer)? {
      return Err(parameter_error(
        env,
        "BLOB parameter must use an ArrayBuffer",
      ));
    }
  }
  // N-API can return a null data pointer for empty or detached buffers.
  if len == 0 {
    return Ok(Some(Vec::new()));
  }
  if data.is_null() || len > isize::MAX as usize {
    return Err(parameter_error(env, "invalid BLOB parameter"));
  }
  Ok(Some(
    std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec(),
  ))
}

fn decode_string(env: sys::napi_env, value: sys::napi_value) -> Result<String> {
  unsafe {
    let mut len = 0;
    check_status!(sys::napi_get_value_string_utf8(
      env,
      value,
      ptr::null_mut(),
      0,
      &mut len
    ))?;
    let mut buf = vec![0u8; len + 1];
    let mut written = 0;
    check_status!(sys::napi_get_value_string_utf8(
      env,
      value,
      buf.as_mut_ptr().cast(),
      buf.len(),
      &mut written,
    ))?;
    buf.truncate(written);
    String::from_utf8(buf).map_err(|err| Error::new(Status::InvalidArg, err.to_string()))
  }
}

fn decode_single(env: &Env, value: sys::napi_value) -> Result<SqlValue> {
  let raw = env.raw();
  match napi_type(raw, value)? {
    sys::ValueType::napi_undefined | sys::ValueType::napi_null => Ok(SqlValue::Null),
    sys::ValueType::napi_boolean => {
      let mut boolean = false;
      check_status!(unsafe { sys::napi_get_value_bool(raw, value, &mut boolean) })?;
      Ok(SqlValue::Integer(i64::from(boolean)))
    }
    sys::ValueType::napi_number => {
      let mut number = 0.0;
      check_status!(unsafe { sys::napi_get_value_double(raw, value, &mut number) })?;
      // i64::MAX rounds to 2^63 as an f64, so the upper bound is exclusive.
      if number.fract() == 0.0 && number >= i64::MIN as f64 && number < i64::MAX as f64 {
        Ok(SqlValue::Integer(number as i64))
      } else {
        Ok(SqlValue::Real(number))
      }
    }
    sys::ValueType::napi_bigint => {
      let mut integer = 0;
      let mut lossless = false;
      check_status!(unsafe {
        sys::napi_get_value_bigint_int64(raw, value, &mut integer, &mut lossless)
      })?;
      if lossless {
        Ok(SqlValue::Integer(integer))
      } else {
        Err(parameter_error(
          env,
          "bigint parameter is outside the SQLite int64 range",
        ))
      }
    }
    sys::ValueType::napi_string => decode_string(raw, value).map(SqlValue::Text),
    sys::ValueType::napi_object => unsafe { extract_bytes(env, value) }?
      .map(SqlValue::Blob)
      .ok_or_else(|| parameter_error(env, "unsupported parameter object")),
    _ => Err(parameter_error(env, "unsupported parameter type")),
  }
}

fn decode_single_or_named(env: &Env, value: sys::napi_value) -> Result<BindParams> {
  if napi_type(env.raw(), value)? == sys::ValueType::napi_object {
    if let Some(bytes) = unsafe { extract_bytes(env, value) }? {
      return Ok(BindParams::Positional(vec![SqlValue::Blob(bytes)]));
    }
    if !is_array(env.raw(), value)? {
      return decode_named(env, value);
    }
  }
  Ok(BindParams::Positional(vec![decode_single(env, value)?]))
}

fn decode_params(env: &Env, params: Option<sys::napi_value>) -> Result<BindParams> {
  let Some(value) = params else {
    return Ok(BindParams::Positional(Vec::new()));
  };
  let raw = env.raw();
  if matches!(
    napi_type(raw, value)?,
    sys::ValueType::napi_undefined | sys::ValueType::napi_null
  ) {
    return Ok(BindParams::Positional(Vec::new()));
  }
  if !is_array(raw, value)? {
    return decode_single_or_named(env, value);
  }
  let mut length = 0;
  check_status!(unsafe { sys::napi_get_array_length(raw, value, &mut length) })?;
  // The bundled SQLite's maximum variable number is 32766.
  if length > 32_766 {
    return Err(parameter_error(env, "too many SQLite parameters"));
  }
  let get = |index| {
    let mut element = ptr::null_mut();
    check_pending_exception!(raw, unsafe {
      sys::napi_get_element(raw, value, index, &mut element)
    })?;
    Ok(element)
  };
  if length == 1 {
    return decode_single_or_named(env, get(0)?);
  }
  let mut out = Vec::with_capacity(length as usize);
  for index in 0..length {
    out.push(decode_single(env, get(index)?)?);
  }
  Ok(BindParams::Positional(out))
}

fn decode_named(env: &Env, object: sys::napi_value) -> Result<BindParams> {
  let raw = env.raw();
  let mut names = ptr::null_mut();
  check_pending_exception!(raw, unsafe {
    sys::napi_get_all_property_names(
      raw,
      object,
      sys::KeyCollectionMode::own_only,
      sys::KeyFilter::enumerable | sys::KeyFilter::skip_symbols,
      sys::KeyConversion::numbers_to_strings,
      &mut names,
    )
  })?;
  let mut length = 0;
  check_status!(unsafe { sys::napi_get_array_length(raw, names, &mut length) })?;
  if length > 32_766 {
    return Err(parameter_error(env, "too many SQLite parameters"));
  }
  let mut out = HashMap::with_capacity(length as usize);
  for i in 0..length {
    let mut key = ptr::null_mut();
    check_status!(unsafe { sys::napi_get_element(raw, names, i, &mut key) })?;
    let name = decode_string(raw, key)?;
    let mut element = ptr::null_mut();
    check_pending_exception!(raw, unsafe {
      sys::napi_get_property(raw, object, key, &mut element)
    })?;
    out.insert(name, decode_single(env, element)?);
  }
  Ok(BindParams::Named(out))
}

// ---------------------------------------------------------------------------
// Query execution on the worker thread
// ---------------------------------------------------------------------------

struct ExecReq {
  sql: String,
  params: BindParams,
  cached: bool,
  values: bool,
  raw: bool,
  safe_integers: bool,
  savepoints: Vec<String>,
}

enum QueryResult {
  Rows {
    names: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
    arrays: bool,
    safe: bool,
  },
  Raw {
    changes: i64,
    last_rowid: i64,
    safe: bool,
  },
  Empty,
}

fn exec_err(err: rusqlite::Error) -> SqlErr {
  SqlErr::sqlite("execute", err)
}

fn bind_params(stmt: &mut Statement, params: &BindParams) -> std::result::Result<(), SqlErr> {
  match params {
    BindParams::Positional(values) => {
      for (index, value) in values.iter().enumerate() {
        stmt
          .raw_bind_parameter(index + 1, value)
          .map_err(exec_err)?;
      }
    }
    BindParams::Named(entries) => {
      let count = stmt.parameter_count();
      for index in 1..=count {
        let Some(name) = stmt.parameter_name(index) else {
          continue;
        };
        // node:sqlite: ":x" matches object keys ":x" or bare "x".
        let bare = &name[1..];
        let value = entries
          .get(name)
          .or_else(|| entries.get(bare))
          .unwrap_or(&SqlValue::Null);
        stmt.raw_bind_parameter(index, value).map_err(exec_err)?;
      }
    }
  }
  Ok(())
}

fn run_statement(
  conn: &Connection,
  stmt: &mut Statement,
  req: &ExecReq,
) -> std::result::Result<QueryResult, SqlErr> {
  bind_params(stmt, &req.params)?;
  let column_count = stmt.column_count();
  if column_count > 0 {
    let names = if req.values {
      Vec::new()
    } else {
      stmt.column_names().map(str::to_owned).collect()
    };
    let mut rows = Vec::new();
    {
      let mut cursor = stmt.raw_query();
      while let Some(row) = cursor.next().map_err(exec_err)? {
        let mut values = Vec::with_capacity(column_count);
        for i in 0..column_count {
          values.push(match row.get_ref(i).map_err(exec_err)? {
            ValueRef::Null => SqlValue::Null,
            ValueRef::Integer(i) => SqlValue::Integer(i),
            ValueRef::Real(f) => SqlValue::Real(f),
            ValueRef::Text(t) => SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
          });
        }
        rows.push(values);
      }
    }
    Ok(QueryResult::Rows {
      names,
      rows,
      arrays: req.values,
      safe: req.safe_integers,
    })
  } else {
    stmt.raw_execute().map_err(exec_err)?;
    if req.raw {
      Ok(QueryResult::Raw {
        changes: conn.changes() as i64,
        last_rowid: conn.last_insert_rowid(),
        safe: req.safe_integers,
      })
    } else {
      Ok(QueryResult::Empty)
    }
  }
}

fn execute_request(conn: &Connection, req: &ExecReq) -> std::result::Result<QueryResult, SqlErr> {
  for savepoint in &req.savepoints {
    conn
      .execute_batch(savepoint)
      .map_err(|e| SqlErr::sqlite("execute", e))?;
  }
  if req.cached {
    let mut stmt = conn
      .prepare_cached(&req.sql)
      .map_err(|e| SqlErr::sqlite("prepare", e))?;
    match run_statement(conn, &mut stmt, req) {
      Ok(result) => Ok(result),
      Err(err) => {
        // A failed step leaves the statement active; evict so the next use
        // re-prepares instead of replaying stale error state.
        stmt.discard();
        Err(err)
      }
    }
  } else {
    let mut stmt = conn
      .prepare(&req.sql)
      .map_err(|e| SqlErr::sqlite("prepare", e))?;
    run_statement(conn, &mut stmt, req)
  }
}

// ---------------------------------------------------------------------------
// QueryResult -> JS conversion (runs on the JS thread via JsDeferred resolver)
// ---------------------------------------------------------------------------

unsafe fn int_value(env: sys::napi_env, value: i64, safe: bool) -> Result<sys::napi_value> {
  let mut out = ptr::null_mut();
  if safe {
    check_status!(
      sys::napi_create_bigint_int64(env, value, &mut out),
      "napi_create_bigint_int64 failed",
    )?;
  } else {
    check_status!(
      sys::napi_create_double(env, value as f64, &mut out),
      "napi_create_double failed",
    )?;
  }
  Ok(out)
}

unsafe fn sql_to_napi(env: sys::napi_env, value: &SqlValue, safe: bool) -> Result<sys::napi_value> {
  match value {
    SqlValue::Null => {
      let mut out = ptr::null_mut();
      check_status!(sys::napi_get_null(env, &mut out), "napi_get_null failed")?;
      Ok(out)
    }
    SqlValue::Integer(i) => int_value(env, *i, safe),
    SqlValue::Real(f) => {
      let mut out = ptr::null_mut();
      check_status!(
        sys::napi_create_double(env, *f, &mut out),
        "napi_create_double failed",
      )?;
      Ok(out)
    }
    SqlValue::Text(text) => make_string(env, text),
    SqlValue::Blob(bytes) => {
      let mut arraybuffer = ptr::null_mut();
      let mut data = ptr::null_mut();
      check_status!(
        sys::napi_create_arraybuffer(env, bytes.len(), &mut data, &mut arraybuffer),
        "napi_create_arraybuffer failed",
      )?;
      if !bytes.is_empty() {
        ptr::copy_nonoverlapping(bytes.as_ptr(), data as *mut u8, bytes.len());
      }
      let mut out = ptr::null_mut();
      check_status!(
        sys::napi_create_typedarray(
          env,
          sys::TypedarrayType::uint8_array,
          bytes.len(),
          arraybuffer,
          0,
          &mut out,
        ),
        "napi_create_typedarray failed",
      )?;
      Ok(out)
    }
  }
}

unsafe fn make_string(env: sys::napi_env, text: &str) -> Result<sys::napi_value> {
  let mut out = ptr::null_mut();
  check_status!(
    sys::napi_create_string_utf8(env, text.as_ptr().cast(), text.len() as isize, &mut out),
    "napi_create_string_utf8 failed",
  )?;
  Ok(out)
}

impl ToNapiValue for QueryResult {
  unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
    match val {
      QueryResult::Empty => {
        let mut out = ptr::null_mut();
        check_status!(
          sys::napi_create_array_with_length(env, 0, &mut out),
          "napi_create_array_with_length failed",
        )?;
        Ok(out)
      }
      QueryResult::Raw {
        changes,
        last_rowid,
        safe,
      } => {
        let mut out = ptr::null_mut();
        check_status!(
          sys::napi_create_object(env, &mut out),
          "napi_create_object failed"
        )?;
        let changes_key = make_string(env, "changes")?;
        let changes_val = int_value(env, changes, safe)?;
        check_status!(
          sys::napi_set_property(env, out, changes_key, changes_val),
          "napi_set_property failed",
        )?;
        let rowid_key = make_string(env, "lastInsertRowid")?;
        let rowid_val = int_value(env, last_rowid, safe)?;
        check_status!(
          sys::napi_set_property(env, out, rowid_key, rowid_val),
          "napi_set_property failed",
        )?;
        Ok(out)
      }
      QueryResult::Rows {
        names,
        rows,
        arrays,
        safe,
      } => {
        let mut out = ptr::null_mut();
        check_status!(
          sys::napi_create_array_with_length(env, rows.len(), &mut out),
          "napi_create_array_with_length failed",
        )?;
        let mut properties = names
          .iter()
          .map(|name| {
            Ok(sys::napi_property_descriptor {
              name: make_string(env, name)?,
              utf8name: ptr::null(),
              method: None,
              getter: None,
              setter: None,
              value: ptr::null_mut(),
              attributes: sys::PropertyAttributes::writable
                | sys::PropertyAttributes::enumerable
                | sys::PropertyAttributes::configurable,
              data: ptr::null_mut(),
            })
          })
          .collect::<Result<Vec<_>>>()?;
        let scope = Env::from_raw(env);
        for (row_index, row) in rows.iter().enumerate() {
          scope.run_in_scope(|| {
            let mut item = ptr::null_mut();
            if arrays {
              check_status!(sys::napi_create_array_with_length(
                env,
                row.len(),
                &mut item
              ))?;
              for (col_index, value) in row.iter().enumerate() {
                let element = sql_to_napi(env, value, safe)?;
                check_status!(sys::napi_set_element(env, item, col_index as u32, element))?;
              }
            } else {
              check_status!(sys::napi_create_object(env, &mut item))?;
              for (property, value) in properties.iter_mut().zip(row) {
                property.value = sql_to_napi(env, value, safe)?;
              }
              // SQL column names are data, including inherited JS property names.
              check_status!(sys::napi_define_properties(
                env,
                item,
                properties.len(),
                properties.as_ptr()
              ))?;
            }
            check_status!(sys::napi_set_element(env, out, row_index as u32, item))
          })?;
        }
        Ok(out)
      }
    }
  }
}

// ---------------------------------------------------------------------------
// NativeSqliteWorker
// ---------------------------------------------------------------------------

enum Job {
  Execute {
    req: ExecReq,
    deferred: JsDeferred<QueryResult, Resolve<QueryResult>>,
  },
  Close,
}

fn open_connection(
  filename: &str,
  read_only: bool,
  require_wal: bool,
  prepare_cache_size: usize,
  cache_size_kb: u32,
) -> std::result::Result<Connection, SqlErr> {
  let conn = if read_only {
    Connection::open_with_flags(
      filename,
      OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
  } else {
    Connection::open(filename)
  }
  .map_err(|e| SqlErr::sqlite("open", e))?;
  conn
    .execute_batch(&format!(
      "PRAGMA busy_timeout = 5000; PRAGMA cache_size = -{cache_size_kb};"
    ))
    .map_err(|e| SqlErr::sqlite("open", e))?;
  conn.set_prepared_statement_cache_capacity(prepare_cache_size);
  // Reads the header, so a file that is not a database fails here rather than on first use.
  let mode: String = conn
    .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
    .map_err(|e| SqlErr::sqlite("open", e))?
    .to_lowercase();
  // Without WAL a reader's shared lock blocks the writer's commits, so refuse.
  if require_wal && mode != "wal" {
    return Err(SqlErr::not_wal(&mode));
  }
  Ok(conn)
}

fn worker_loop(conn: Connection, rx: Receiver<Job>) {
  for job in rx {
    match job {
      Job::Execute { req, deferred } => {
        settle(deferred, execute_request(&conn, &req));
      }
      Job::Close => {
        // Dropping the connection lets SQLite checkpoint and remove the WAL.
        drop(conn);
        return;
      }
    }
  }
}

#[napi]
pub struct NativeSqliteWorker {
  sender: Mutex<Option<Sender<Job>>>,
  join: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[napi]
impl NativeSqliteWorker {
  #[napi(constructor)]
  pub fn new() -> Result<Self> {
    Err(Error::new(
      Status::InvalidArg,
      "use NativeSqliteWorker.open()".to_owned(),
    ))
  }
}

/// `NativeSqliteWorker.open(options)` — implemented as a free function because
/// factories must return `Self` synchronously, and `open` resolves a promise.
#[napi(js_name = "openSqliteWorker")]
pub fn open_sqlite_worker(env: Env, options: Object) -> Result<NapiValue> {
  let filename = options
    .get::<String>("filename")?
    .ok_or_else(|| Error::new(Status::InvalidArg, "filename is required".to_owned()))?;
  let read_only = options.get::<bool>("readOnly")?.unwrap_or(false);
  let require_wal = options.get::<bool>("requireWal")?.unwrap_or(false);
  let prepare_cache_size = options.get::<u32>("prepareCacheSize")?.unwrap_or(200) as usize;
  // Readers keep 4 MiB of page cache, the writer keeps 8 MiB.
  let cache_size_kb =
    options
      .get::<u32>("cacheSizeKb")?
      .unwrap_or(if read_only { 4000 } else { 8000 });

  let (deferred, promise) =
    env.create_deferred::<NativeSqliteWorker, Resolve<NativeSqliteWorker>>()?;
  let join_slot = Arc::new(Mutex::new(None::<JoinHandle<()>>));
  let join_for_worker = join_slot.clone();
  let (tx, rx) = channel::<Job>();

  let handle = thread::Builder::new()
    .name("t3-native-sqlite-worker".to_owned())
    .spawn(move || {
      match open_connection(
        &filename,
        read_only,
        require_wal,
        prepare_cache_size,
        cache_size_kb,
      ) {
        Ok(conn) => {
          deferred.resolve(Box::new(move |_env| {
            Ok(NativeSqliteWorker {
              sender: Mutex::new(Some(tx)),
              join: join_for_worker,
            })
          }));
          worker_loop(conn, rx);
        }
        Err(err) => deferred.resolve(Box::new(move |env| Err(sql_err_to_napi(&env, err)))),
      }
    })
    .map_err(|e| Error::new(Status::GenericFailure, format!("spawn failed: {e}")))?;
  *join_slot.lock().expect("join lock poisoned") = Some(handle);
  Ok(NapiValue(promise.raw()))
}

#[napi]
impl NativeSqliteWorker {
  #[napi]
  pub fn execute(&self, env: Env, request: Object) -> Result<NapiValue> {
    let sql = request
      .get::<String>("sql")?
      .ok_or_else(|| Error::new(Status::InvalidArg, "sql is required".to_owned()))?;
    let params_value = request.get::<Unknown>("params")?.map(|value| value.raw());
    let req = ExecReq {
      sql,
      params: decode_params(&env, params_value)?,
      cached: request.get::<bool>("cached")?.unwrap_or(false),
      values: request.get::<bool>("values")?.unwrap_or(false),
      raw: request.get::<bool>("raw")?.unwrap_or(false),
      safe_integers: request.get::<bool>("safeIntegers")?.unwrap_or(false),
      savepoints: request
        .get::<Vec<String>>("savepoints")?
        .unwrap_or_default(),
    };
    let (deferred, promise) = env.create_deferred::<QueryResult, Resolve<QueryResult>>()?;
    let job = Job::Execute { req, deferred };
    let sender = self.sender.lock().expect("sender lock poisoned");
    let returned = match sender.as_ref() {
      Some(tx) => tx.send(job).err().map(|err| err.0),
      None => Some(job),
    };
    if let Some(Job::Execute { deferred, .. }) = returned {
      settle(
        deferred,
        Err(SqlErr::plain("execute", "database worker is closed")),
      );
    }
    Ok(NapiValue(promise.raw()))
  }

  #[napi]
  pub fn close(&self, env: Env) -> Result<NapiValue> {
    let (deferred, promise) = env.create_deferred::<(), Resolve<()>>()?;
    let sender = self.sender.lock().expect("sender lock poisoned").take();
    let handle = self.join.lock().expect("join lock poisoned").take();
    if let Some(tx) = sender {
      let _ = tx.send(Job::Close);
    }
    match handle {
      Some(handle) => {
        thread::spawn(move || {
          let _ = handle.join();
          deferred.resolve(Box::new(|_| Ok(())));
        });
      }
      None => deferred.resolve(Box::new(|_| Ok(()))),
    }
    Ok(NapiValue(promise.raw()))
  }
}

// ---------------------------------------------------------------------------
// NativeCheckpointWorker
// ---------------------------------------------------------------------------

struct CkptShared {
  notified: bool,
  stop: bool,
}

#[napi]
pub struct NativeCheckpointWorker {
  state: Arc<(Mutex<CkptShared>, Condvar)>,
  join: Mutex<Option<JoinHandle<()>>>,
  last_error: Arc<Mutex<Option<String>>>,
}

fn checkpoint_state(conn: &Connection, pragma: &str) -> rusqlite::Result<(i64, i64)> {
  let mut statement = conn.prepare_cached(pragma)?;
  statement.query_row([], |row| Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
}

/// Returns Ok(true) while the WAL still has un-checkpointed frames and an idle
/// re-check should run (mirrors `check(force)` in `nodeSqliteClient.ts`).
fn checkpoint_once(conn: &Connection, frames: i64, force: bool) -> rusqlite::Result<bool> {
  let before = checkpoint_state(conn, "PRAGMA wal_checkpoint(NOOP)")?;
  let ran_checkpoint = before.0 - before.1 >= frames || (force && before.0 > before.1);
  let state = if ran_checkpoint {
    checkpoint_state(conn, "PRAGMA wal_checkpoint(PASSIVE)")?
  } else {
    before
  };
  // The WAL restarts only once every frame is checkpointed, so catch up when
  // writes pause. A reader pinning the WAL stops progress; the next write retries.
  let progressed = !force || state.1 > before.1;
  let more = state.0 > state.1 && progressed;
  if ran_checkpoint && !more {
    // Nothing left to checkpoint; hand the page cache back before sleeping.
    conn.execute_batch("PRAGMA shrink_memory")?;
  }
  Ok(more)
}

fn checkpoint_loop(
  filename: String,
  frames: i64,
  idle: Duration,
  state: Arc<(Mutex<CkptShared>, Condvar)>,
  last_error: Arc<Mutex<Option<String>>>,
) {
  let fail = |message: String| {
    *last_error.lock().expect("last_error lock poisoned") = Some(message);
  };
  if rusqlite::version_number() < 3_051_000 {
    fail(format!(
      "SQLite {} has no NOOP checkpoint",
      rusqlite::version()
    ));
    return;
  }
  let conn = match Connection::open_with_flags(&filename, OpenFlags::SQLITE_OPEN_READ_WRITE) {
    Ok(conn) => conn,
    Err(err) => {
      fail(err.to_string());
      return;
    }
  };
  if let Err(err) = conn.execute_batch(
    "PRAGMA busy_timeout = 5000; PRAGMA checkpoint_fullfsync = ON; PRAGMA cache_size = -512;",
  ) {
    fail(err.to_string());
    return;
  }
  let (lock, cv) = &*state;
  let mut pending_idle: Option<Instant> = None;
  loop {
    let mut guard = lock.lock().expect("checkpoint lock poisoned");
    let mut force = false;
    loop {
      if guard.stop {
        return;
      }
      if guard.notified {
        guard.notified = false;
        break;
      }
      match pending_idle {
        Some(deadline) => {
          let now = Instant::now();
          if now >= deadline {
            force = true;
            break;
          }
          guard = cv
            .wait_timeout(guard, deadline - now)
            .expect("cv poisoned")
            .0;
        }
        None => {
          guard = cv.wait(guard).expect("cv poisoned");
        }
      }
    }
    drop(guard);
    pending_idle = None;
    match checkpoint_once(&conn, frames, force) {
      Ok(more) => {
        if more {
          pending_idle = Some(Instant::now() + idle);
        }
      }
      Err(err) => {
        fail(err.to_string());
        return;
      }
    }
  }
}

impl NativeCheckpointWorker {
  fn request_stop(&self) {
    let (lock, cv) = &*self.state;
    let mut guard = lock.lock().expect("checkpoint lock poisoned");
    guard.stop = true;
    cv.notify_one();
  }
}

impl Drop for NativeCheckpointWorker {
  fn drop(&mut self) {
    self.request_stop();
  }
}

#[napi]
impl NativeCheckpointWorker {
  #[napi(constructor)]
  pub fn new() -> Result<Self> {
    Err(Error::new(
      Status::InvalidArg,
      "use NativeCheckpointWorker.start()".to_owned(),
    ))
  }

  #[napi(factory)]
  pub fn start(options: Object) -> Result<Self> {
    let filename = options
      .get::<String>("filename")?
      .ok_or_else(|| Error::new(Status::InvalidArg, "filename is required".to_owned()))?;
    let frames = options.get::<u32>("frames")?.unwrap_or(1000) as i64;
    let idle_ms = options.get::<u32>("idleMs")?.unwrap_or(100);

    let shared = Arc::new((
      Mutex::new(CkptShared {
        notified: false,
        stop: false,
      }),
      Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None::<String>));
    let handle = thread::Builder::new()
      .name("t3-native-sqlite-checkpoint".to_owned())
      .spawn({
        let shared = shared.clone();
        let last_error = last_error.clone();
        move || {
          checkpoint_loop(
            filename,
            frames,
            Duration::from_millis(idle_ms as u64),
            shared,
            last_error,
          )
        }
      })
      .map_err(|e| Error::new(Status::GenericFailure, format!("spawn failed: {e}")))?;
    Ok(NativeCheckpointWorker {
      state: shared,
      join: Mutex::new(Some(handle)),
      last_error,
    })
  }

  #[napi]
  pub fn notify(&self) {
    let (lock, cv) = &*self.state;
    let mut guard = lock.lock().expect("checkpoint lock poisoned");
    guard.notified = true;
    cv.notify_one();
  }

  #[napi]
  pub fn stop(&self, env: Env) -> Result<NapiValue> {
    self.request_stop();
    let (deferred, promise) = env.create_deferred::<(), Resolve<()>>()?;
    match self.join.lock().expect("join lock poisoned").take() {
      Some(handle) => {
        thread::spawn(move || {
          let _ = handle.join();
          deferred.resolve(Box::new(|_| Ok(())));
        });
      }
      None => deferred.resolve(Box::new(|_| Ok(()))),
    }
    Ok(NapiValue(promise.raw()))
  }

  /// The last fatal error recorded by the checkpoint thread, if any.
  #[napi]
  pub fn last_error(&self) -> Option<String> {
    self
      .last_error
      .lock()
      .expect("last_error lock poisoned")
      .clone()
  }
}

// ---------------------------------------------------------------------------
// Version helper
// ---------------------------------------------------------------------------

#[napi]
pub fn sqlite_version() -> String {
  rusqlite::version().to_owned()
}
