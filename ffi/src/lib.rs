// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

#![allow(clippy::missing_safety_doc)]

use paimon_ftindex_core::io::{ReadRequest, SeekRead, SeekWrite};
use paimon_ftindex_core::{
    FullTextIndexConfig, FullTextIndexReader, FullTextIndexWriter, FullTextReadMetrics,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::{ptr, slice};

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(msg: impl Into<String>) {
    let msg = msg.into().replace('\0', "\\0");
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(msg).ok();
    });
}

fn panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<String>() {
        format!("native panic: {s}")
    } else if let Some(s) = e.downcast_ref::<&str>() {
        format!("native panic: {s}")
    } else {
        "native panic: unknown".to_string()
    }
}

fn ffi_status<F>(f: F) -> c_int
where
    F: FnOnce() -> Result<(), String>,
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            set_error(e);
            -1
        }
        Err(e) => {
            set_error(panic_message(&e));
            -1
        }
    }
}

fn ffi_ptr<T, F>(f: F) -> *mut T
where
    F: FnOnce() -> Result<*mut T, String>,
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            set_error(e);
            ptr::null_mut()
        }
        Err(e) => {
            set_error(panic_message(&e));
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn paimon_ftindex_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(msg) => msg.as_ptr(),
        None => ptr::null(),
    })
}

#[repr(C)]
pub struct PaimonFtindexOutputFile {
    pub ctx: *mut c_void,
    pub write_fn: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int>,
    pub flush_fn: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}

struct FfiOutputFile {
    raw: PaimonFtindexOutputFile,
}

unsafe impl Send for FfiOutputFile {}

impl SeekWrite for FfiOutputFile {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let write_fn = self
            .raw
            .write_fn
            .ok_or_else(|| io::Error::other("write_fn is null"))?;
        let status = unsafe { write_fn(self.raw.ctx, buf.as_ptr(), buf.len()) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::other("write callback failed"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(flush_fn) = self.raw.flush_fn {
            let status = unsafe { flush_fn(self.raw.ctx) };
            if status != 0 {
                return Err(io::Error::other("flush callback failed"));
            }
        }
        Ok(())
    }
}

#[repr(C)]
pub struct PaimonFtindexInputFile {
    pub ctx: *mut c_void,
    pub pread_fn: Option<unsafe extern "C" fn(*mut c_void, u64, *mut u8, usize) -> c_int>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PaimonFtindexReadMetrics {
    pub pread_calls: u64,
    pub pread_ranges: u64,
    pub pread_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_evictions: u64,
    pub cached_blocks: u64,
}

impl From<FullTextReadMetrics> for PaimonFtindexReadMetrics {
    fn from(metrics: FullTextReadMetrics) -> Self {
        Self {
            pread_calls: metrics.pread_calls,
            pread_ranges: metrics.pread_ranges,
            pread_bytes: metrics.pread_bytes,
            cache_hits: metrics.cache_hits,
            cache_misses: metrics.cache_misses,
            cache_evictions: metrics.cache_evictions,
            cached_blocks: metrics.cached_blocks,
        }
    }
}

struct FfiInputFile {
    raw: PaimonFtindexInputFile,
}

unsafe impl Send for FfiInputFile {}
unsafe impl Sync for FfiInputFile {}

impl SeekRead for FfiInputFile {
    fn pread(&self, ranges: &mut [ReadRequest<'_>]) -> io::Result<()> {
        let pread_fn = self
            .raw
            .pread_fn
            .ok_or_else(|| io::Error::other("pread_fn is null"))?;
        for range in ranges {
            let status = unsafe {
                pread_fn(
                    self.raw.ctx,
                    range.pos,
                    range.buf.as_mut_ptr(),
                    range.buf.len(),
                )
            };
            if status != 0 {
                return Err(io::Error::other(format!(
                    "pread callback failed at offset {} length {}",
                    range.pos,
                    range.buf.len()
                )));
            }
        }
        Ok(())
    }
}

pub struct PaimonFtindexWriterHandle {
    inner: FullTextIndexWriter,
}

pub struct PaimonFtindexReaderHandle {
    inner: FullTextIndexReader<FfiInputFile>,
}

struct SearchRequest<'a> {
    query: *const c_char,
    limit: usize,
    roaring_filter: Option<&'a [u8]>,
    row_ids: *mut i64,
    scores: *mut f32,
    capacity: usize,
    result_len: *mut usize,
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_writer_open(
    keys: *const *const c_char,
    values: *const *const c_char,
    len: usize,
) -> *mut PaimonFtindexWriterHandle {
    ffi_ptr(|| {
        let options = options_from_raw(keys, values, len)?;
        let config = FullTextIndexConfig::from_options(&options).map_err(|e| e.to_string())?;
        let writer = FullTextIndexWriter::new(config).map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(PaimonFtindexWriterHandle {
            inner: writer,
        })))
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_writer_add_document(
    writer: *mut PaimonFtindexWriterHandle,
    row_id: i64,
    text: *const c_char,
) -> c_int {
    ffi_status(|| {
        let writer = require_mut(writer, "writer")?;
        let text = cstr_to_string(text, "text")?;
        writer
            .inner
            .add_document(row_id, text)
            .map_err(|e| e.to_string())
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_writer_add_document_fields(
    writer: *mut PaimonFtindexWriterHandle,
    row_id: i64,
    field_names: *const *const c_char,
    texts: *const *const c_char,
    len: usize,
) -> c_int {
    ffi_status(|| {
        let writer = require_mut(writer, "writer")?;
        let fields = fields_from_raw(field_names, texts, len)?;
        writer
            .inner
            .add_document_fields(row_id, fields)
            .map_err(|e| e.to_string())
    })
}

#[no_mangle]
/// Finalizes the writer and writes its archive to `output`.
///
/// The writer is finalized after any call, including calls that return a non-zero status. Callers
/// must discard a potentially partial output and create a new writer to retry.
pub unsafe extern "C" fn paimon_ftindex_writer_write_index(
    writer: *mut PaimonFtindexWriterHandle,
    output: PaimonFtindexOutputFile,
) -> c_int {
    ffi_status(|| {
        let writer = require_mut(writer, "writer")?;
        let mut output = FfiOutputFile { raw: output };
        writer.inner.write(&mut output).map_err(|e| e.to_string())
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_writer_free(writer: *mut PaimonFtindexWriterHandle) {
    if !writer.is_null() {
        drop(Box::from_raw(writer));
    }
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_open(
    input: PaimonFtindexInputFile,
) -> *mut PaimonFtindexReaderHandle {
    ffi_ptr(|| {
        let input = FfiInputFile { raw: input };
        let reader = FullTextIndexReader::open(input).map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(PaimonFtindexReaderHandle {
            inner: reader,
        })))
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_search(
    reader: *mut PaimonFtindexReaderHandle,
    query: *const c_char,
    limit: usize,
    row_ids: *mut i64,
    scores: *mut f32,
    capacity: usize,
    result_len: *mut usize,
) -> c_int {
    ffi_status(|| {
        let reader = require_ref(reader, "reader")?;
        search_impl(
            reader,
            SearchRequest {
                query,
                limit,
                roaring_filter: None,
                row_ids,
                scores,
                capacity,
                result_len,
            },
        )
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_search_with_roaring_filter(
    reader: *mut PaimonFtindexReaderHandle,
    query: *const c_char,
    limit: usize,
    roaring_filter: *const u8,
    roaring_filter_len: usize,
    row_ids: *mut i64,
    scores: *mut f32,
    capacity: usize,
    result_len: *mut usize,
) -> c_int {
    ffi_status(|| {
        let reader = require_ref(reader, "reader")?;
        let roaring_filter = const_slice(roaring_filter, roaring_filter_len, "roaring_filter")?;
        search_impl(
            reader,
            SearchRequest {
                query,
                limit,
                roaring_filter: Some(roaring_filter),
                row_ids,
                scores,
                capacity,
                result_len,
            },
        )
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_prewarm(
    reader: *mut PaimonFtindexReaderHandle,
) -> c_int {
    ffi_status(|| {
        let reader = require_ref(reader, "reader")?;
        reader.inner.prewarm().map_err(|e| e.to_string())
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_read_metrics(
    reader: *mut PaimonFtindexReaderHandle,
    metrics: *mut PaimonFtindexReadMetrics,
) -> c_int {
    ffi_status(|| {
        let reader = require_ref(reader, "reader")?;
        let metrics = require_mut(metrics, "metrics")?;
        *metrics = reader.inner.read_metrics().into();
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn paimon_ftindex_reader_free(reader: *mut PaimonFtindexReaderHandle) {
    if !reader.is_null() {
        drop(Box::from_raw(reader));
    }
}

unsafe fn options_from_raw(
    keys: *const *const c_char,
    values: *const *const c_char,
    len: usize,
) -> Result<HashMap<String, String>, String> {
    if len == 0 {
        return Ok(HashMap::new());
    }
    if keys.is_null() {
        return Err("keys is null".to_string());
    }
    if values.is_null() {
        return Err("values is null".to_string());
    }
    let keys = slice::from_raw_parts(keys, len);
    let values = slice::from_raw_parts(values, len);
    let mut options = HashMap::with_capacity(len);
    for i in 0..len {
        let key = cstr_to_string(keys[i], "option key")?;
        let value = cstr_to_string(values[i], "option value")?;
        options.insert(key, value);
    }
    Ok(options)
}

unsafe fn fields_from_raw(
    field_names: *const *const c_char,
    texts: *const *const c_char,
    len: usize,
) -> Result<Vec<(String, String)>, String> {
    if len == 0 {
        return Err("document fields must not be empty".to_string());
    }
    if field_names.is_null() {
        return Err("field_names is null".to_string());
    }
    if texts.is_null() {
        return Err("texts is null".to_string());
    }
    let field_names = slice::from_raw_parts(field_names, len);
    let texts = slice::from_raw_parts(texts, len);
    let mut fields = Vec::with_capacity(len);
    for i in 0..len {
        let field_name = cstr_to_string(field_names[i], "field name")?;
        let text = cstr_to_string(texts[i], "field text")?;
        fields.push((field_name, text));
    }
    Ok(fields)
}

unsafe fn require_mut<'a, T>(ptr: *mut T, name: &str) -> Result<&'a mut T, String> {
    ptr.as_mut().ok_or_else(|| format!("{name} is null"))
}

unsafe fn require_ref<'a, T>(ptr: *const T, name: &str) -> Result<&'a T, String> {
    ptr.as_ref().ok_or_else(|| format!("{name} is null"))
}

unsafe fn const_slice<'a, T>(ptr: *const T, len: usize, name: &str) -> Result<&'a [T], String> {
    if len == 0 {
        Ok(&[])
    } else if ptr.is_null() {
        Err(format!("{name} is null"))
    } else {
        Ok(slice::from_raw_parts(ptr, len))
    }
}

unsafe fn cstr_to_string(ptr: *const c_char, name: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("{name} is null"));
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map(|s| s.to_string())
        .map_err(|e| format!("{name} is not valid UTF-8: {e}"))
}

fn search_impl(
    reader: &PaimonFtindexReaderHandle,
    request: SearchRequest<'_>,
) -> Result<(), String> {
    if request.result_len.is_null() {
        return Err("result_len is null".to_string());
    }
    let query = unsafe { cstr_to_string(request.query, "query") }?;
    let result = if let Some(roaring_filter) = request.roaring_filter {
        reader
            .inner
            .search_with_roaring_filter(&query, request.limit, roaring_filter)
            .map_err(|e| e.to_string())?
    } else {
        reader
            .inner
            .search(&query, request.limit)
            .map_err(|e| e.to_string())?
    };
    unsafe {
        *request.result_len = result.row_ids.len();
    }
    if result.row_ids.len() > request.capacity {
        return Err(format!(
            "result capacity {} is smaller than result length {}",
            request.capacity,
            result.row_ids.len()
        ));
    }
    if !result.row_ids.is_empty() {
        if request.row_ids.is_null() {
            return Err("row_ids is null".to_string());
        }
        if request.scores.is_null() {
            return Err("scores is null".to_string());
        }
        unsafe {
            ptr::copy_nonoverlapping(
                result.row_ids.as_ptr(),
                request.row_ids,
                result.row_ids.len(),
            );
            ptr::copy_nonoverlapping(result.scores.as_ptr(), request.scores, result.scores.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringTreemap;
    use std::ffi::CString;

    unsafe extern "C" fn write_vec(ctx: *mut c_void, buf: *const u8, len: usize) -> c_int {
        let out = &mut *(ctx as *mut Vec<u8>);
        out.extend_from_slice(slice::from_raw_parts(buf, len));
        0
    }

    unsafe extern "C" fn read_vec(ctx: *mut c_void, pos: u64, buf: *mut u8, len: usize) -> c_int {
        let input = &*(ctx as *const Vec<u8>);
        let start = pos as usize;
        let end = start + len;
        if end > input.len() {
            return -1;
        }
        ptr::copy_nonoverlapping(input[start..end].as_ptr(), buf, len);
        0
    }

    #[test]
    fn ffi_round_trip_search() {
        unsafe {
            let writer = paimon_ftindex_writer_open(ptr::null(), ptr::null(), 0);
            assert!(!writer.is_null());

            let text = CString::new("Apache Paimon full text").unwrap();
            assert_eq!(
                paimon_ftindex_writer_add_document(writer, 7, text.as_ptr()),
                0
            );

            let mut bytes = Vec::new();
            let output = PaimonFtindexOutputFile {
                ctx: &mut bytes as *mut Vec<u8> as *mut c_void,
                write_fn: Some(write_vec),
                flush_fn: None,
            };
            assert_eq!(paimon_ftindex_writer_write_index(writer, output), 0);
            paimon_ftindex_writer_free(writer);

            let input = PaimonFtindexInputFile {
                ctx: &bytes as *const Vec<u8> as *mut c_void,
                pread_fn: Some(read_vec),
            };
            let reader = paimon_ftindex_reader_open(input);
            assert!(!reader.is_null());

            let mut metrics = PaimonFtindexReadMetrics::default();
            assert_eq!(paimon_ftindex_reader_read_metrics(reader, &mut metrics), 0);
            assert!(metrics.pread_calls >= 2);
            assert!(metrics.pread_bytes > 16);
            assert_eq!(paimon_ftindex_reader_prewarm(reader), 0);
            let mut after_prewarm = PaimonFtindexReadMetrics::default();
            assert_eq!(
                paimon_ftindex_reader_read_metrics(reader, &mut after_prewarm),
                0
            );
            assert!(after_prewarm.pread_calls > metrics.pread_calls);

            let query = CString::new(r#"{"match":{"query":"paimon","column":"text"}}"#).unwrap();
            let mut row_ids = [0i64; 4];
            let mut scores = [0f32; 4];
            let mut result_len = 0usize;
            assert_eq!(
                paimon_ftindex_reader_search(
                    reader,
                    query.as_ptr(),
                    4,
                    row_ids.as_mut_ptr(),
                    scores.as_mut_ptr(),
                    row_ids.len(),
                    &mut result_len,
                ),
                0
            );
            assert_eq!(result_len, 1);
            assert_eq!(row_ids[0], 7);
            assert!(scores[0] > 0.0);
            let mut after_search = PaimonFtindexReadMetrics::default();
            assert_eq!(
                paimon_ftindex_reader_read_metrics(reader, &mut after_search),
                0
            );
            assert!(after_search.pread_calls >= after_prewarm.pread_calls);
            assert!(after_search.cache_misses >= metrics.cache_misses);
            paimon_ftindex_reader_free(reader);
        }
    }

    #[test]
    fn ffi_round_trip_search_with_roaring_filter() {
        unsafe {
            let writer = paimon_ftindex_writer_open(ptr::null(), ptr::null(), 0);
            assert!(!writer.is_null());

            let text = CString::new("Apache Paimon full text").unwrap();
            assert_eq!(
                paimon_ftindex_writer_add_document(writer, 7, text.as_ptr()),
                0
            );
            let text = CString::new("Paimon filtered row").unwrap();
            assert_eq!(
                paimon_ftindex_writer_add_document(writer, 9, text.as_ptr()),
                0
            );

            let mut bytes = Vec::new();
            let output = PaimonFtindexOutputFile {
                ctx: &mut bytes as *mut Vec<u8> as *mut c_void,
                write_fn: Some(write_vec),
                flush_fn: None,
            };
            assert_eq!(paimon_ftindex_writer_write_index(writer, output), 0);
            paimon_ftindex_writer_free(writer);

            let input = PaimonFtindexInputFile {
                ctx: &bytes as *const Vec<u8> as *mut c_void,
                pread_fn: Some(read_vec),
            };
            let reader = paimon_ftindex_reader_open(input);
            assert!(!reader.is_null());

            let query = CString::new(r#"{"match":{"query":"paimon","column":"text"}}"#).unwrap();
            let mut allowed = RoaringTreemap::new();
            allowed.insert(9);
            let mut filter_bytes = Vec::new();
            allowed.serialize_into(&mut filter_bytes).unwrap();
            let mut row_ids = [0i64; 4];
            let mut scores = [0f32; 4];
            let mut result_len = 0usize;
            assert_eq!(
                paimon_ftindex_reader_search_with_roaring_filter(
                    reader,
                    query.as_ptr(),
                    4,
                    filter_bytes.as_ptr(),
                    filter_bytes.len(),
                    row_ids.as_mut_ptr(),
                    scores.as_mut_ptr(),
                    row_ids.len(),
                    &mut result_len,
                ),
                0
            );
            assert_eq!(result_len, 1);
            assert_eq!(row_ids[0], 9);
            assert!(scores[0] > 0.0);
            paimon_ftindex_reader_free(reader);
        }
    }

    #[test]
    fn ffi_add_document_fields_searches_multi_field_index() {
        unsafe {
            let key = CString::new("text-fields").unwrap();
            let value = CString::new("title,body").unwrap();
            let keys = [key.as_ptr()];
            let values = [value.as_ptr()];
            let writer = paimon_ftindex_writer_open(keys.as_ptr(), values.as_ptr(), keys.len());
            assert!(!writer.is_null());

            let title = CString::new("title").unwrap();
            let body = CString::new("body").unwrap();
            let title_text = CString::new("Apache Paimon").unwrap();
            let body_text = CString::new("lake storage").unwrap();
            let field_names = [title.as_ptr(), body.as_ptr()];
            let texts = [title_text.as_ptr(), body_text.as_ptr()];
            assert_eq!(
                paimon_ftindex_writer_add_document_fields(
                    writer,
                    17,
                    field_names.as_ptr(),
                    texts.as_ptr(),
                    field_names.len(),
                ),
                0
            );

            let mut bytes = Vec::new();
            let output = PaimonFtindexOutputFile {
                ctx: &mut bytes as *mut Vec<u8> as *mut c_void,
                write_fn: Some(write_vec),
                flush_fn: None,
            };
            assert_eq!(paimon_ftindex_writer_write_index(writer, output), 0);
            paimon_ftindex_writer_free(writer);

            let input = PaimonFtindexInputFile {
                ctx: &bytes as *const Vec<u8> as *mut c_void,
                pread_fn: Some(read_vec),
            };
            let reader = paimon_ftindex_reader_open(input);
            assert!(!reader.is_null());

            let query =
                CString::new(r#"{"multi_match":{"query":"paimon","columns":["title","body"]}}"#)
                    .unwrap();
            let mut row_ids = [0i64; 4];
            let mut scores = [0f32; 4];
            let mut result_len = 0usize;
            assert_eq!(
                paimon_ftindex_reader_search(
                    reader,
                    query.as_ptr(),
                    4,
                    row_ids.as_mut_ptr(),
                    scores.as_mut_ptr(),
                    row_ids.len(),
                    &mut result_len,
                ),
                0
            );
            assert_eq!(result_len, 1);
            assert_eq!(row_ids[0], 17);
            paimon_ftindex_reader_free(reader);
        }
    }
}
