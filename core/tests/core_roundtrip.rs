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

use paimon_ftindex_core::io::{PosWriter, ReadRequest, SeekRead, SeekWrite, SliceReader};
use paimon_ftindex_core::storage::{read_header, write_envelope, ArchiveFileEntry, IndexHeader};
use paimon_ftindex_core::{
    FullTextIndexConfig, FullTextIndexMetadata, FullTextIndexReader, FullTextIndexWriter,
    TokenizerConfig, TokenizerKind,
};
use roaring::RoaringTreemap;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

fn build_index() -> anyhow::Result<Vec<u8>> {
    build_index_with_config(FullTextIndexConfig::new())
}

fn build_index_with_config(config: FullTextIndexConfig) -> anyhow::Result<Vec<u8>> {
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document(10, "Apache Paimon supports full text search")?;
    writer.add_document(11, "Tantivy is a Rust search engine")?;
    writer.add_document(12, "Paimon tables can use indexes")?;

    let mut bytes = Vec::new();
    {
        let mut output = PosWriter::new(&mut bytes);
        writer.write(&mut output)?;
    }
    Ok(bytes)
}

fn match_query(terms: &str, column: &str) -> String {
    serde_json::json!({
        "match": {
            "query": terms,
            "column": column,
        }
    })
    .to_string()
}

fn match_query_without_column(terms: &str) -> String {
    serde_json::json!({
        "match": {
            "query": terms,
        }
    })
    .to_string()
}

fn match_query_and(terms: &str, column: &str) -> String {
    serde_json::json!({
        "match": {
            "query": terms,
            "column": column,
            "operator": "AND",
        }
    })
    .to_string()
}

fn fuzzy_match_query(
    terms: &str,
    column: &str,
    fuzziness: u8,
    max_expansions: usize,
    prefix_length: u32,
) -> String {
    serde_json::json!({
        "match": {
            "query": terms,
            "column": column,
            "fuzziness": fuzziness,
            "max_expansions": max_expansions,
            "prefix_length": prefix_length,
        }
    })
    .to_string()
}

fn phrase_query(terms: &str, column: &str) -> String {
    serde_json::json!({
        "match_phrase": {
            "query": terms,
            "column": column,
        }
    })
    .to_string()
}

#[derive(Default)]
struct ReadStats {
    pread_calls: usize,
    max_ranges_per_batch: usize,
    total_bytes_read: usize,
}

struct CountingSliceReader {
    data: Vec<u8>,
    stats: Arc<Mutex<ReadStats>>,
}

impl CountingSliceReader {
    fn new(data: Vec<u8>, stats: Arc<Mutex<ReadStats>>) -> Self {
        Self { data, stats }
    }
}

impl SeekRead for CountingSliceReader {
    fn pread(&self, ranges: &mut [ReadRequest<'_>]) -> io::Result<()> {
        {
            let mut stats = self.stats.lock().unwrap();
            stats.pread_calls += 1;
            stats.max_ranges_per_batch = stats.max_ranges_per_batch.max(ranges.len());
            stats.total_bytes_read += ranges.iter().map(|range| range.buf.len()).sum::<usize>();
        }
        for range in ranges {
            let start = usize::try_from(range.pos)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
            let end = start
                .checked_add(range.buf.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "range overflow"))?;
            if end > self.data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read past end of slice",
                ));
            }
            range.buf.copy_from_slice(&self.data[start..end]);
        }
        Ok(())
    }
}

struct ChunkLimitedWriter {
    data: Vec<u8>,
    max_chunk_len: usize,
}

impl ChunkLimitedWriter {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            max_chunk_len: 0,
        }
    }
}

impl SeekWrite for ChunkLimitedWriter {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        const MAX_CHUNK_LEN: usize = 64 * 1024;
        if buf.len() > MAX_CHUNK_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive write chunk exceeds {MAX_CHUNK_LEN} bytes"),
            ));
        }
        self.max_chunk_len = self.max_chunk_len.max(buf.len());
        self.data.extend_from_slice(buf);
        Ok(())
    }
}

struct FailingWriter;

impl SeekWrite for FailingWriter {
    fn write_all(&mut self, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::other("intentional output failure"))
    }
}

#[test]
fn failed_write_attempt_finalizes_writer() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "Apache Paimon full text")?;

    let write_error = writer
        .write(&mut FailingWriter)
        .expect_err("the failing output should reject the first write attempt");
    assert!(write_error
        .to_string()
        .contains("intentional output failure"));

    let mut retry_output = ChunkLimitedWriter::new();
    let retry_error = writer
        .write(&mut retry_output)
        .expect_err("a failed write attempt should still finalize the writer");
    assert!(retry_error.to_string().contains("already finalized"));

    let add_error = writer
        .add_document(2, "retry document")
        .expect_err("a finalized writer should reject additional documents");
    assert!(add_error.to_string().contains("already finalized"));
    Ok(())
}

#[test]
fn large_incremental_archive_is_streamed_and_searchable() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    for row_id in 0..10_000 {
        writer.add_document(
            row_id,
            format!("common archive text with unique marker{row_id}"),
        )?;
    }

    let mut output = ChunkLimitedWriter::new();
    writer.write(&mut output)?;

    assert!(output.max_chunk_len <= 64 * 1024);
    let reader = FullTextIndexReader::open(SliceReader::new(output.data))?;
    assert_eq!(reader.metadata().document_count, 10_000);
    let result = reader.search(match_query("marker8191", "text"), 10)?;
    assert_eq!(result.row_ids, vec![8191]);
    Ok(())
}

#[test]
fn reader_open_does_not_load_all_archive_files() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let (header, body_start) = read_header(&SliceReader::new(bytes.clone()))?;
    let body_len = header
        .files
        .iter()
        .map(|file| file.offset + file.length)
        .max()
        .unwrap_or(0) as usize;
    let stats = Arc::new(Mutex::new(ReadStats::default()));
    let input = CountingSliceReader::new(bytes, Arc::clone(&stats));

    let reader = FullTextIndexReader::open(input)?;
    assert_eq!(reader.metadata().document_count, 3);
    let bytes_read_at_open = stats.lock().unwrap().total_bytes_read;
    let header_bytes = usize::try_from(body_start)?;
    assert!(
        bytes_read_at_open < header_bytes + body_len,
        "reader open should not read the complete Tantivy archive body"
    );
    let result = reader.search(match_query("paimon", "text"), 10)?;
    assert_eq!(result.row_ids.len(), 2);
    Ok(())
}

#[test]
fn repeated_search_reuses_tantivy_reader_io() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let stats = Arc::new(Mutex::new(ReadStats::default()));
    let input = CountingSliceReader::new(bytes, Arc::clone(&stats));

    let reader = FullTextIndexReader::open(input)?;
    let calls_after_open = stats.lock().unwrap().pread_calls;
    let result = reader.search(match_query("missing", "text"), 10)?;
    assert!(result.row_ids.is_empty());
    let calls_after_first_search = stats.lock().unwrap().pread_calls;
    let result = reader.search(match_query("missing", "text"), 10)?;
    assert!(result.row_ids.is_empty());
    let calls_after_second_search = stats.lock().unwrap().pread_calls;

    let first_search_calls = calls_after_first_search - calls_after_open;
    let second_search_calls = calls_after_second_search - calls_after_first_search;
    assert!(
        second_search_calls < first_search_calls,
        "second search should reuse Tantivy reader metadata; first search used {first_search_calls} pread calls, second used {second_search_calls}"
    );
    Ok(())
}

#[test]
fn prewarm_initializes_tantivy_reader_before_first_search() -> anyhow::Result<()> {
    let bytes = build_index()?;

    let cold_stats = Arc::new(Mutex::new(ReadStats::default()));
    let cold_reader = FullTextIndexReader::open(CountingSliceReader::new(
        bytes.clone(),
        Arc::clone(&cold_stats),
    ))?;
    let cold_calls_after_open = cold_stats.lock().unwrap().pread_calls;
    let cold_result = cold_reader.search(match_query("missing", "text"), 10)?;
    assert!(cold_result.row_ids.is_empty());
    let cold_calls_after_search = cold_stats.lock().unwrap().pread_calls;
    let cold_first_search_calls = cold_calls_after_search - cold_calls_after_open;

    let warm_stats = Arc::new(Mutex::new(ReadStats::default()));
    let warm_reader =
        FullTextIndexReader::open(CountingSliceReader::new(bytes, Arc::clone(&warm_stats)))?;
    let warm_calls_after_open = warm_stats.lock().unwrap().pread_calls;
    warm_reader.prewarm()?;
    let warm_calls_after_prewarm = warm_stats.lock().unwrap().pread_calls;
    assert!(
        warm_calls_after_prewarm > warm_calls_after_open,
        "prewarm should eagerly initialize Tantivy reader I/O"
    );

    let warm_result = warm_reader.search(match_query("missing", "text"), 10)?;
    assert!(warm_result.row_ids.is_empty());
    let warm_calls_after_search = warm_stats.lock().unwrap().pread_calls;
    let warm_first_search_calls = warm_calls_after_search - warm_calls_after_prewarm;

    assert!(
        warm_first_search_calls < cold_first_search_calls,
        "prewarm should move first-search reader I/O earlier; cold first search used {cold_first_search_calls} pread calls, warm first search used {warm_first_search_calls}"
    );
    Ok(())
}

#[test]
fn read_metrics_report_pread_and_cache_activity() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;

    let after_open = reader.read_metrics();
    assert!(after_open.pread_calls >= 2);
    assert!(after_open.pread_bytes > 16);

    let result = reader.search(match_query("missing", "text"), 10)?;
    assert!(result.row_ids.is_empty());
    let after_first_search = reader.read_metrics();
    assert!(after_first_search.pread_calls > after_open.pread_calls);
    assert!(after_first_search.pread_bytes > after_open.pread_bytes);
    assert!(after_first_search.cache_misses > after_open.cache_misses);
    assert!(after_first_search.cached_blocks > 0);

    Ok(())
}

#[test]
fn match_query_round_trip() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(match_query("paimon", "text"), 10)?;

    assert_eq!(reader.metadata().document_count, 3);
    assert_eq!(result.row_ids.len(), 2);
    assert!(result.row_ids.contains(&10));
    assert!(result.row_ids.contains(&12));
    assert_eq!(result.scores.len(), 2);
    Ok(())
}

#[test]
fn written_header_has_single_source_of_truth_for_format_and_fields() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let header_len = u32::from_be_bytes(bytes[12..16].try_into()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(&bytes[16..16 + header_len])?;

    assert!(header.get("format_version").is_none());
    assert!(header["metadata"].get("format_version").is_none());
    assert!(header["metadata"]["config"].get("text_field").is_none());
    assert_eq!(
        header["metadata"]["config"]["text_fields"],
        serde_json::json!(["text"])
    );
    Ok(())
}

#[test]
fn storage_v1_envelope_matches_golden_file() -> anyhow::Result<()> {
    let header = IndexHeader {
        metadata: FullTextIndexMetadata {
            config: FullTextIndexConfig::new(),
            document_count: 2,
            tantivy_version: "0.26.0".to_string(),
        },
        files: vec![
            ArchiveFileEntry {
                name: "meta.json".to_string(),
                offset: 0,
                length: 2,
            },
            ArchiveFileEntry {
                name: "segment.idx".to_string(),
                offset: 2,
                length: 4,
            },
        ],
    };
    let files = vec![
        ("meta.json".to_string(), b"ab".to_vec()),
        ("segment.idx".to_string(), b"cdef".to_vec()),
    ];

    let mut bytes = Vec::new();
    write_envelope(&mut PosWriter::new(&mut bytes), &header, &files)?;

    let expected = decode_hex(include_str!("golden/storage_v1_envelope.hex"))?;
    assert_eq!(bytes, expected);

    let (actual_header, body_start) = read_header(&SliceReader::new(bytes.clone()))?;
    assert_eq!(actual_header, header);
    assert_eq!(&bytes[body_start as usize..], b"abcdef");
    Ok(())
}

#[test]
fn reader_rejects_mismatched_tantivy_version() {
    let mut bytes = Vec::new();
    let header = IndexHeader {
        metadata: FullTextIndexMetadata {
            config: FullTextIndexConfig::new(),
            document_count: 0,
            tantivy_version: "0.0.0".to_string(),
        },
        files: vec![ArchiveFileEntry {
            name: "meta.json".to_string(),
            offset: 0,
            length: 0,
        }],
    };
    write_envelope(
        &mut PosWriter::new(&mut bytes),
        &header,
        &[("meta.json".to_string(), Vec::new())],
    )
    .expect("write envelope");

    let err = match FullTextIndexReader::open(SliceReader::new(bytes)) {
        Ok(_) => panic!("Tantivy version mismatch should fail"),
        Err(err) => err,
    };

    assert!(err
        .to_string()
        .contains("unsupported Tantivy index version 0.0.0"));
}

fn decode_hex(hex: &str) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut high = None;
    for ch in hex.chars().filter(|ch| !ch.is_whitespace()) {
        let value = ch
            .to_digit(16)
            .ok_or_else(|| anyhow::anyhow!("invalid hex digit: {ch}"))? as u8;
        if let Some(previous) = high.take() {
            bytes.push((previous << 4) | value);
        } else {
            high = Some(value);
        }
    }
    if high.is_some() {
        anyhow::bail!("hex input has an odd number of digits");
    }
    Ok(bytes)
}

#[test]
fn match_query_and_operator_filters_terms() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = match_query_and("paimon indexes", "text");
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![12]);
    Ok(())
}

#[test]
fn search_with_roaring_filter_limits_allowed_row_ids_before_top_docs() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let query = match_query("paimon", "text");

    let reader = FullTextIndexReader::open(SliceReader::new(bytes.clone()))?;
    let unfiltered_top = reader.search(query.clone(), 1)?.row_ids[0];
    let allowed_id = if unfiltered_top == 10 { 12 } else { 10 };

    let mut allowed = RoaringTreemap::new();
    allowed.insert(allowed_id as u64);
    let mut filter_bytes = Vec::new();
    allowed.serialize_into(&mut filter_bytes)?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search_with_roaring_filter(query, 1, &filter_bytes)?;

    assert_eq!(result.row_ids, vec![allowed_id]);
    assert_eq!(result.scores.len(), 1);
    Ok(())
}

#[test]
fn search_with_empty_roaring_filter_returns_empty_results() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let empty = RoaringTreemap::new();
    let mut filter_bytes = Vec::new();
    empty.serialize_into(&mut filter_bytes)?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result =
        reader.search_with_roaring_filter(match_query("paimon", "text"), 10, &filter_bytes)?;

    assert!(result.row_ids.is_empty());
    assert!(result.scores.is_empty());
    Ok(())
}

#[test]
fn search_with_roaring_filter_supports_64_bit_row_ids() -> anyhow::Result<()> {
    let allowed_id = (1i64 << 33) + 17;
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "apache paimon")?;
    writer.add_document(allowed_id, "paimon filtered row")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let mut allowed = RoaringTreemap::new();
    allowed.insert(allowed_id as u64);
    let mut filter_bytes = Vec::new();
    allowed.serialize_into(&mut filter_bytes)?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result =
        reader.search_with_roaring_filter(match_query("paimon", "text"), 10, &filter_bytes)?;

    assert_eq!(result.row_ids, vec![allowed_id]);
    Ok(())
}

#[test]
fn search_rejects_invalid_roaring_filter_bytes() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let err = reader
        .search_with_roaring_filter(match_query("paimon", "text"), 10, b"not roaring")
        .expect_err("invalid filter bytes should fail");

    assert!(err.to_string().contains("invalid RoaringTreemap filter"));
    Ok(())
}

#[test]
fn phrase_query_uses_positions() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(phrase_query("full text", "text"), 10)?;

    assert_eq!(result.row_ids, vec![10]);
    Ok(())
}

#[test]
fn phrase_query_rejects_index_without_positions() -> anyhow::Result<()> {
    let bytes = build_index_with_config(FullTextIndexConfig::new().with_positions(false))?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let err = reader
        .search(phrase_query("full text", "text"), 10)
        .expect_err("phrase query without positions should fail");

    assert!(err.to_string().contains("phrase query requires positions"));
    Ok(())
}

#[test]
fn jieba_tokenizer_searches_chinese_terms() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().tokenizer(TokenizerConfig {
        tokenizer: TokenizerKind::Jieba,
        ..TokenizerConfig::default()
    });
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document(20, "中华人民共和国人民大会堂")?;
    writer.add_document(21, "北京大学支持全文检索")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(match_query("中华", "text"), 10)?;

    assert_eq!(result.row_ids, vec![20]);
    Ok(())
}

#[test]
fn jieba_tokenizer_supports_chinese_phrase_queries() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().tokenizer(TokenizerConfig {
        tokenizer: TokenizerKind::Jieba,
        jieba_ordinal_position: true,
        with_position: true,
        ..TokenizerConfig::default()
    });
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document(30, "北京大学支持全文检索")?;
    writer.add_document(31, "北京的大学很多")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(phrase_query("北京大学", "text"), 10)?;

    assert_eq!(result.row_ids, vec![30]);
    Ok(())
}

#[test]
fn tokenizer_options_parse_jieba_settings() -> anyhow::Result<()> {
    let mut options = HashMap::new();
    options.insert("fulltext.tokenizer".to_string(), "jieba".to_string());
    options.insert(
        "fulltext.jieba.search-mode".to_string(),
        "false".to_string(),
    );
    options.insert(
        "fulltext.jieba.ordinal-position".to_string(),
        "false".to_string(),
    );

    let config = TokenizerConfig::from_options(&options)?;

    assert_eq!(config.tokenizer, TokenizerKind::Jieba);
    assert!(!config.jieba_search_mode);
    assert!(!config.jieba_ordinal_position);
    Ok(())
}

#[test]
fn tokenizer_options_parse_ngram_and_stop_word_settings() -> anyhow::Result<()> {
    let mut options = HashMap::new();
    options.insert("fulltext.tokenizer".to_string(), "ngram".to_string());
    options.insert("fulltext.ngram.min-gram".to_string(), "2".to_string());
    options.insert("fulltext.ngram.max-gram".to_string(), "4".to_string());
    options.insert("fulltext.ngram.prefix-only".to_string(), "true".to_string());
    options.insert(
        "fulltext.stop-words".to_string(),
        "apache;paimon".to_string(),
    );

    let config = TokenizerConfig::from_options(&options)?;

    assert_eq!(config.tokenizer, TokenizerKind::Ngram);
    assert_eq!(config.ngram_min_gram, 2);
    assert_eq!(config.ngram_max_gram, 4);
    assert!(config.ngram_prefix_only);
    assert_eq!(config.stop_words, vec!["apache", "paimon"]);
    Ok(())
}

#[test]
fn stop_words_require_stop_word_removal() {
    let mut options = HashMap::new();
    options.insert("remove-stop-words".to_string(), "false".to_string());
    options.insert("stop-words".to_string(), "apache".to_string());

    let err = TokenizerConfig::from_options(&options)
        .expect_err("stop words should require remove-stop-words=true");

    assert!(err.to_string().contains("requires remove-stop-words=true"));
}

#[test]
fn tokenizer_default_values_are_explicit() {
    let config = TokenizerConfig::default();

    assert_eq!(config.tokenizer, TokenizerKind::Default);
    assert_eq!(config.ngram_min_gram, 3);
    assert_eq!(config.ngram_max_gram, 3);
    assert!(!config.ngram_prefix_only);
    assert!(config.lower_case);
    assert_eq!(config.max_token_length, 40);
    assert!(config.ascii_folding);
    assert!(config.stem);
    assert_eq!(config.language, "english");
    assert!(config.remove_stop_words);
    assert!(config.stop_words.is_empty());
    assert!(config.with_position);
}

#[test]
fn boost_query_requires_positive_match() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "apache paimon")?;
    writer.add_document(2, "tantivy only")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = serde_json::json!({
        "boost": {
            "positive": {"match": {"query": "paimon", "column": "text"}},
            "negative": {"match": {"query": "tantivy", "column": "text"}},
            "negative_boost": 0.5,
        }
    })
    .to_string();
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![1]);
    Ok(())
}

#[test]
fn boost_query_demotes_negative_matches() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "paimon good")?;
    writer.add_document(2, "paimon bad")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = serde_json::json!({
        "boost": {
            "positive": {"match": {"query": "paimon", "column": "text"}},
            "negative": {"match": {"query": "bad", "column": "text"}},
            "negative_boost": 0.5,
        }
    })
    .to_string();
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![1, 2]);
    assert!(result.scores[0] > result.scores[1]);
    Ok(())
}

#[test]
fn search_accepts_paimon_match_query_aliases() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(
        r#"{"match":{"column":"text","query":"paimon indexes","operator":"AND","boost":2.0,"fuzziness":0,"maxExpansions":50,"prefixLength":0}}"#,
        10,
    )?;

    assert_eq!(result.row_ids, vec![12]);
    Ok(())
}

#[test]
fn match_query_can_omit_column_for_single_field_index() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = r#"{"match":{"query":"paimon","operator":"OR"}}"#;
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids.len(), 2);
    assert!(result.row_ids.contains(&10));
    assert!(result.row_ids.contains(&12));
    Ok(())
}

#[test]
fn match_query_without_column_searches_all_indexed_fields() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().with_text_fields(["title", "body"]);
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document_fields(
        1,
        [
            ("title".to_string(), "Apache Paimon".to_string()),
            ("body".to_string(), "lake storage".to_string()),
        ],
    )?;
    writer.add_document_fields(
        2,
        [
            ("title".to_string(), "Tantivy".to_string()),
            ("body".to_string(), "Rust search engine".to_string()),
        ],
    )?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = match_query_without_column("rust");
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![2]);
    Ok(())
}

#[test]
fn search_rejects_unknown_column_name() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = r#"{"match":{"column":"content","query":"paimon","operator":"OR"}}"#;
    let err = reader
        .search(query, 10)
        .expect_err("unknown column should fail");

    assert!(err
        .to_string()
        .contains("column 'content' is not configured"));
    Ok(())
}

#[test]
fn search_accepts_paimon_boolean_query_forms() -> anyhow::Result<()> {
    let query = r#"{"boolean":{"must":[{"match":{"column":"text","terms":"paimon"}}],"must_not":[{"phrase":{"column":"text","query":"legacy"}}]}}"#;
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids.len(), 2);
    assert!(result.row_ids.contains(&10));
    assert!(result.row_ids.contains(&12));
    Ok(())
}

#[test]
fn default_tokenizer_stems_removes_stop_words_and_folds_ascii() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "The runners visited cafes")?;
    writer.add_document(2, "plain unrelated text")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(match_query_and("runner café", "text"), 10)?;

    assert_eq!(result.row_ids, vec![1]);
    Ok(())
}

#[test]
fn fuzzy_match_query_matches_typos() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = fuzzy_match_query("paimno", "text", 1, 50, 0);
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids.len(), 2);
    assert!(result.row_ids.contains(&10));
    assert!(result.row_ids.contains(&12));
    Ok(())
}

#[test]
fn fuzzy_match_query_supports_auto_fuzziness_json() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = r#"{"match":{"column":"text","query":"paimxx","fuzziness":"auto"}}"#;
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids.len(), 2);
    Ok(())
}

#[test]
fn fuzzy_prefix_length_requires_exact_start() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "paimon search")?;
    writer.add_document(2, "xaimon search")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes.clone()))?;
    let query = fuzzy_match_query("paimno", "text", 1, 50, 3);
    let result = reader.search(query, 10)?;
    assert_eq!(result.row_ids, vec![1]);

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = fuzzy_match_query("qaimon", "text", 1, 50, 1);
    let result = reader.search(query, 10)?;
    assert!(result.row_ids.is_empty());
    Ok(())
}

#[test]
fn fuzzy_match_query_honors_max_expansions() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "aac")?;
    writer.add_document(2, "abc")?;
    writer.add_document(3, "acc")?;
    writer.add_document(4, "bbc")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let query = |max_expansions| fuzzy_match_query("abc", "text", 1, max_expansions, 0);

    let reader = FullTextIndexReader::open(SliceReader::new(bytes.clone()))?;
    let capped = reader.search(query(1), 10)?;
    assert_eq!(capped.row_ids.len(), 1);

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let expanded = reader.search(query(50), 10)?;
    assert!(expanded.row_ids.len() > capped.row_ids.len());
    assert_eq!(expanded.row_ids.len(), 4);
    Ok(())
}

#[test]
fn multi_field_match_searches_named_fields() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().with_text_fields(["title", "body"]);
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document_fields(
        1,
        [
            ("title".to_string(), "Apache Paimon".to_string()),
            ("body".to_string(), "lake storage".to_string()),
        ],
    )?;
    writer.add_document_fields(
        2,
        [
            ("title".to_string(), "Tantivy".to_string()),
            ("body".to_string(), "Rust search engine".to_string()),
        ],
    )?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(match_query("rust", "body"), 10)?;

    assert_eq!(result.row_ids, vec![2]);
    Ok(())
}

#[test]
fn multi_match_searches_columns_and_applies_boosts() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().with_text_fields(["title", "body"]);
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document_fields(
        1,
        [
            ("title".to_string(), "paimon".to_string()),
            ("body".to_string(), "storage".to_string()),
        ],
    )?;
    writer.add_document_fields(
        2,
        [
            ("title".to_string(), "storage".to_string()),
            ("body".to_string(), "paimon".to_string()),
        ],
    )?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query =
        r#"{"multi_match":{"query":"paimon","columns":["title","body"],"boost":[3.0,1.0]}}"#;
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![1, 2]);
    assert!(result.scores[0] > result.scores[1]);
    Ok(())
}

#[test]
fn repeated_named_fields_support_string_arrays() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().with_text_fields(["tags"]);
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document_fields(
        1,
        [
            ("tags".to_string(), "paimon".to_string()),
            ("tags".to_string(), "lakehouse".to_string()),
        ],
    )?;
    writer.add_document_fields(2, [("tags".to_string(), "tantivy".to_string())])?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(match_query("lakehouse", "tags"), 10)?;

    assert_eq!(result.row_ids, vec![1]);
    Ok(())
}

#[test]
fn boolean_query_rejects_only_must_not() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = r#"{"boolean":{"must_not":[{"match":{"column":"text","query":"paimon"}}]}}"#;
    let err = reader
        .search(query, 10)
        .expect_err("only must_not should fail");

    assert!(err
        .to_string()
        .contains("at least one should or must clause"));
    Ok(())
}
