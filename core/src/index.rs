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

use crate::archive_directory::ArchiveDirectory;
use crate::config::{FullTextIndexConfig, FullTextIndexMetadata};
use crate::error::{FtIndexError, Result};
use crate::io::{FullTextReadMetrics, ReadMetrics, ReadRequest, SeekRead, SeekWrite};
use crate::query::{BooleanOccur, MatchOperator, QuerySpec};
use crate::storage::{read_header, write_envelope_from_paths, ArchiveFileEntry, IndexHeader};
use crate::tokenizer::{TokenizerConfig, TokenizerKind};
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA, SINK_STATE};
use roaring::RoaringTreemap;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tantivy::collector::{FilterCollector, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScorer, EmptyQuery, EnableScoring, Explanation, Occur, Query,
    QueryParser, Scorer, TermQuery, Weight,
};
use tantivy::schema::{IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter,
    SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer, TokenStream, WhitespaceTokenizer,
};
use tantivy::{
    DocId, DocSet, Index, IndexWriter, Score, SegmentReader, TantivyDocument, Term, TERMINATED,
};
use tantivy_fst::Automaton;
use tantivy_jieba::JiebaTokenizer;
use tempfile::TempDir;

const INDEX_WRITER_MEMORY_BUDGET_BYTES: usize = 50_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct FullTextSearchResult {
    pub row_ids: Vec<i64>,
    pub scores: Vec<f32>,
}

pub struct FullTextIndexWriter {
    config: FullTextIndexConfig,
    state: Option<FullTextIndexWriterState>,
    row_id_field: tantivy::schema::Field,
    text_fields: HashMap<String, tantivy::schema::Field>,
    document_count: u64,
}

struct FullTextIndexWriterState {
    index_writer: IndexWriter<TantivyDocument>,
    temp_dir: TempDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullTextDocument {
    pub row_id: i64,
    pub fields: Vec<(String, String)>,
}

impl FullTextIndexWriter {
    pub fn new(config: FullTextIndexConfig) -> Result<Self> {
        config.validate()?;
        let temp_dir = TempDir::new()?;
        let schema = build_schema(&config);
        let mut index = Index::create_in_dir(temp_dir.path(), schema.clone())?;
        register_tokenizer(&mut index, &config.tokenizer)?;
        let row_id_field = schema
            .get_field(&config.row_id_field)
            .map_err(|_| FtIndexError::InvalidStorage("missing row_id field".to_string()))?;
        let text_fields = text_field_map(&schema, &config)?;
        let index_writer = index.writer_with_num_threads(1, INDEX_WRITER_MEMORY_BUDGET_BYTES)?;
        Ok(Self {
            config,
            state: Some(FullTextIndexWriterState {
                index_writer,
                temp_dir,
            }),
            row_id_field,
            text_fields,
            document_count: 0,
        })
    }

    pub fn add_document(&mut self, row_id: i64, text: impl Into<String>) -> Result<()> {
        let text_field = self.config.default_text_field().to_string();
        self.add_document_fields(row_id, [(text_field, text.into())])
    }

    pub fn add_document_fields<I, K, V>(&mut self, row_id: i64, fields: I) -> Result<()>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        if row_id < 0 {
            return Err(FtIndexError::InvalidStorage(format!(
                "row id must be non-negative, got {row_id}"
            )));
        }
        let state = self.state.as_ref().ok_or_else(|| {
            FtIndexError::InvalidStorage("full-text index writer is already finalized".to_string())
        })?;
        let next_document_count = self.document_count.checked_add(1).ok_or_else(|| {
            FtIndexError::InvalidStorage("full-text document count overflow".to_string())
        })?;
        let mut doc = TantivyDocument::new();
        doc.add_u64(self.row_id_field, row_id as u64);
        let mut has_text_field = false;
        for (name, text) in fields {
            let name = name.into();
            let text = text.into();
            validate_indexed_field(&self.config, &name)?;
            let text_field = self.text_fields.get(&name).ok_or_else(|| {
                FtIndexError::InvalidStorage(format!(
                    "document field '{name}' is not configured for this index"
                ))
            })?;
            doc.add_text(*text_field, &text);
            has_text_field = true;
        }
        if !has_text_field {
            return Err(FtIndexError::InvalidStorage(
                "document must contain at least one text field".to_string(),
            ));
        }
        state.index_writer.add_document(doc)?;
        self.document_count = next_document_count;
        Ok(())
    }

    /// Finalizes this writer and streams the completed index archive to `output`.
    ///
    /// A write attempt is single-use regardless of whether it succeeds: the active Tantivy writer
    /// is consumed before commit and serialization begin. After this method is called, subsequent
    /// calls to `write`, `add_document`, or `add_document_fields` return an already-finalized
    /// error. If `output` returns an error, it may contain a partial archive and must be discarded;
    /// retrying requires a new writer and re-adding the documents.
    pub fn write<W: SeekWrite>(&mut self, output: &mut W) -> Result<()> {
        let state = self.state.take().ok_or_else(|| {
            FtIndexError::InvalidStorage("full-text index writer is already finalized".to_string())
        })?;
        let FullTextIndexWriterState {
            mut index_writer,
            temp_dir,
        } = state;
        index_writer.commit()?;
        index_writer.wait_merging_threads()?;

        let files = collect_index_files(temp_dir.path())?;
        let mut offset = 0u64;
        let mut entries = Vec::with_capacity(files.len());
        for file in &files {
            entries.push(ArchiveFileEntry {
                name: file.name.clone(),
                offset,
                length: file.length,
            });
            offset = offset.checked_add(file.length).ok_or_else(|| {
                FtIndexError::InvalidStorage("full-text archive size overflow".to_string())
            })?;
        }

        let header = IndexHeader {
            metadata: FullTextIndexMetadata {
                config: self.config.clone(),
                document_count: self.document_count,
                tantivy_version: tantivy::version().to_string(),
            },
            files: entries,
        };
        let paths = files.iter().map(|file| &file.path).collect::<Vec<_>>();
        write_envelope_from_paths(output, &header, &paths)
    }
}

pub struct FullTextIndexReader<R> {
    _input: Arc<MeteredSeekRead<R>>,
    index: Index,
    reader: Mutex<Option<tantivy::IndexReader>>,
    read_metrics: Arc<ReadMetrics>,
    metadata: FullTextIndexMetadata,
}

impl<R: SeekRead + 'static> FullTextIndexReader<R> {
    pub fn open(input: R) -> Result<Self> {
        let read_metrics = Arc::new(ReadMetrics::default());
        let input = MeteredSeekRead {
            inner: input,
            metrics: Arc::clone(&read_metrics),
        };
        let (header, body_start) = read_header(&input)?;
        validate_tantivy_version(&header.metadata)?;
        let input = Arc::new(input);
        let directory = ArchiveDirectory::new_with_metrics(
            Arc::clone(&input),
            body_start,
            &header.files,
            Arc::clone(&read_metrics),
        )?;
        let mut index = Index::open(directory)?;
        register_tokenizer(&mut index, &header.metadata.config.tokenizer)?;
        Ok(Self {
            _input: input,
            index,
            reader: Mutex::new(None),
            read_metrics,
            metadata: header.metadata,
        })
    }

    pub fn metadata(&self) -> &FullTextIndexMetadata {
        &self.metadata
    }

    pub fn read_metrics(&self) -> FullTextReadMetrics {
        self.read_metrics.snapshot()
    }

    pub fn prewarm(&self) -> Result<()> {
        let _ = self.searcher()?;
        Ok(())
    }

    pub fn search<Q: AsRef<str>>(&self, query: Q, limit: usize) -> Result<FullTextSearchResult> {
        self.search_with_filter(query, limit, None)
    }

    pub fn search_with_roaring_filter<Q: AsRef<str>>(
        &self,
        query: Q,
        limit: usize,
        roaring_filter_bytes: &[u8],
    ) -> Result<FullTextSearchResult> {
        let filter = decode_roaring_filter(roaring_filter_bytes)?;
        self.search_with_filter(query, limit, Some(filter))
    }

    fn search_with_filter<Q: AsRef<str>>(
        &self,
        query: Q,
        limit: usize,
        filter: Option<RoaringTreemap>,
    ) -> Result<FullTextSearchResult> {
        if limit == 0 {
            return Err(FtIndexError::InvalidQuery(
                "search limit must be positive".to_string(),
            ));
        }
        let searcher = self.searcher()?;
        let query = QuerySpec::from_json(query.as_ref())?;
        let tantivy_query = build_query(&self.index, &self.metadata.config, &query)?;
        let top_docs = if let Some(filter) = filter {
            let row_id_field = self.metadata.config.row_id_field.clone();
            let collector = FilterCollector::new(
                row_id_field,
                move |row_id: u64| filter.contains(row_id),
                TopDocs::with_limit(limit).order_by_score(),
            );
            searcher.search(&tantivy_query, &collector)?
        } else {
            searcher.search(&tantivy_query, &TopDocs::with_limit(limit).order_by_score())?
        };
        let mut row_ids = Vec::with_capacity(top_docs.len());
        let mut scores = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let segment_reader = searcher.segment_reader(doc_address.segment_ord);
            let row_id_column = segment_reader
                .fast_fields()
                .u64(&self.metadata.config.row_id_field)?
                .first_or_default_col(0);
            row_ids.push(row_id_column.get_val(doc_address.doc_id) as i64);
            scores.push(score);
        }
        Ok(FullTextSearchResult { row_ids, scores })
    }

    fn searcher(&self) -> Result<tantivy::Searcher> {
        let mut reader = self.reader.lock().map_err(|_| {
            FtIndexError::InvalidStorage("Tantivy reader lock poisoned".to_string())
        })?;
        if reader.is_none() {
            *reader = Some(self.index.reader()?);
        }
        Ok(reader.as_ref().expect("reader initialized").searcher())
    }
}

struct MeteredSeekRead<R> {
    inner: R,
    metrics: Arc<ReadMetrics>,
}

impl<R: SeekRead> SeekRead for MeteredSeekRead<R> {
    fn pread(&self, ranges: &mut [ReadRequest<'_>]) -> std::io::Result<()> {
        self.metrics.record_pread(ranges);
        self.inner.pread(ranges)
    }
}

fn validate_tantivy_version(metadata: &FullTextIndexMetadata) -> Result<()> {
    let runtime_version = tantivy::version().to_string();
    if metadata.tantivy_version != runtime_version {
        return Err(FtIndexError::InvalidStorage(format!(
            "unsupported Tantivy index version {}, runtime uses {}",
            metadata.tantivy_version, runtime_version
        )));
    }
    Ok(())
}

fn decode_roaring_filter(bytes: &[u8]) -> Result<RoaringTreemap> {
    RoaringTreemap::deserialize_from(bytes)
        .map_err(|e| FtIndexError::InvalidQuery(format!("invalid RoaringTreemap filter: {e}")))
}

fn build_schema(config: &FullTextIndexConfig) -> Schema {
    let mut builder = Schema::builder();
    builder.add_u64_field(
        &config.row_id_field,
        NumericOptions::default()
            .set_fast()
            .set_stored()
            .set_indexed(),
    );
    let index_option = if config.tokenizer.with_position {
        IndexRecordOption::WithFreqsAndPositions
    } else {
        IndexRecordOption::WithFreqs
    };
    let tokenizer_name = tokenizer_name(&config.tokenizer);
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(tokenizer_name)
        .set_index_option(index_option);
    for field in config.indexed_text_fields() {
        builder.add_text_field(
            field,
            TextOptions::default().set_indexing_options(indexing.clone()),
        );
    }
    builder.build()
}

fn tokenizer_name(config: &TokenizerConfig) -> &'static str {
    match config.tokenizer {
        TokenizerKind::Default | TokenizerKind::Simple => "paimon_custom",
        TokenizerKind::Whitespace => "paimon_custom",
        TokenizerKind::Raw => "paimon_custom",
        TokenizerKind::Ngram => "paimon_ngram",
        TokenizerKind::Jieba => "paimon_jieba",
    }
}

fn register_tokenizer(index: &mut Index, config: &TokenizerConfig) -> Result<()> {
    let analyzer = build_text_analyzer(config)?;
    index
        .tokenizers()
        .register(tokenizer_name(config), analyzer);
    Ok(())
}

fn build_text_analyzer(config: &TokenizerConfig) -> Result<TextAnalyzer> {
    let mut builder = match config.tokenizer {
        TokenizerKind::Default | TokenizerKind::Simple => {
            TextAnalyzer::builder(SimpleTokenizer::default()).dynamic()
        }
        TokenizerKind::Whitespace => {
            TextAnalyzer::builder(WhitespaceTokenizer::default()).dynamic()
        }
        TokenizerKind::Raw => TextAnalyzer::builder(RawTokenizer::default()).dynamic(),
        TokenizerKind::Ngram => {
            let tokenizer = NgramTokenizer::new(
                config.ngram_min_gram,
                config.ngram_max_gram,
                config.ngram_prefix_only,
            )
            .map_err(|e| FtIndexError::InvalidOption {
                key: "ngram".to_string(),
                message: e.to_string(),
            })?;
            TextAnalyzer::builder(tokenizer).dynamic()
        }
        TokenizerKind::Jieba => {
            let mut tokenizer = JiebaTokenizer::with_search_mode(config.jieba_search_mode);
            tokenizer.set_ordinal_position_mode(config.jieba_ordinal_position);
            TextAnalyzer::builder(tokenizer).dynamic()
        }
    };
    builder = builder.filter_dynamic(RemoveLongFilter::limit(config.max_token_length));
    if config.lower_case {
        builder = builder.filter_dynamic(LowerCaser);
    }
    if config.stem {
        builder = builder.filter_dynamic(Stemmer::new(parse_language(&config.language)?));
    }
    if config.remove_stop_words {
        let language = parse_language(&config.language)?;
        if let Some(filter) = StopWordFilter::new(language) {
            builder = builder.filter_dynamic(filter);
        } else if config.stop_words.is_empty() {
            return Err(FtIndexError::InvalidOption {
                key: "language".to_string(),
                message: format!(
                    "removing stop words for language '{}' is not supported",
                    config.language
                ),
            });
        }
        if !config.stop_words.is_empty() {
            builder = builder.filter_dynamic(StopWordFilter::remove(config.stop_words.clone()));
        }
    }
    if config.ascii_folding {
        builder = builder.filter_dynamic(AsciiFoldingFilter);
    }
    Ok(builder.build())
}

fn parse_language(language: &str) -> Result<Language> {
    match language.trim().to_lowercase().as_str() {
        "arabic" => Ok(Language::Arabic),
        "danish" => Ok(Language::Danish),
        "dutch" => Ok(Language::Dutch),
        "english" | "en" => Ok(Language::English),
        "finnish" => Ok(Language::Finnish),
        "french" | "fr" => Ok(Language::French),
        "german" | "de" => Ok(Language::German),
        "greek" => Ok(Language::Greek),
        "hungarian" => Ok(Language::Hungarian),
        "italian" | "it" => Ok(Language::Italian),
        "norwegian" => Ok(Language::Norwegian),
        "portuguese" | "pt" => Ok(Language::Portuguese),
        "romanian" => Ok(Language::Romanian),
        "russian" | "ru" => Ok(Language::Russian),
        "spanish" | "es" => Ok(Language::Spanish),
        "swedish" => Ok(Language::Swedish),
        "tamil" => Ok(Language::Tamil),
        "turkish" => Ok(Language::Turkish),
        other => Err(FtIndexError::InvalidOption {
            key: "language".to_string(),
            message: format!("unsupported tokenizer language '{other}'"),
        }),
    }
}

fn text_field_map(
    schema: &Schema,
    config: &FullTextIndexConfig,
) -> Result<HashMap<String, tantivy::schema::Field>> {
    let mut fields = HashMap::new();
    for name in config.indexed_text_fields() {
        let field = schema
            .get_field(name)
            .map_err(|_| FtIndexError::InvalidStorage(format!("missing text field '{name}'")))?;
        fields.insert(name.to_string(), field);
    }
    Ok(fields)
}

fn validate_indexed_field(config: &FullTextIndexConfig, field: &str) -> Result<()> {
    if field.trim().is_empty() {
        return Err(FtIndexError::InvalidStorage(
            "document field name must not be empty".to_string(),
        ));
    }
    if config.indexed_text_fields().contains(&field) {
        Ok(())
    } else {
        Err(FtIndexError::InvalidStorage(format!(
            "document field '{field}' is not configured for this index"
        )))
    }
}

struct IndexFile {
    name: String,
    path: PathBuf,
    length: u64,
}

fn collect_index_files(path: &Path) -> Result<Vec<IndexFile>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| FtIndexError::InvalidStorage("non-utf8 file name".to_string()))?
            .to_string();
        if name.ends_with(".lock") {
            continue;
        }
        let length = fs::metadata(&path)?.len();
        files.push(IndexFile { name, path, length });
    }
    Ok(files)
}

fn build_query(
    index: &Index,
    config: &FullTextIndexConfig,
    query: &QuerySpec,
) -> Result<Box<dyn Query>> {
    match query {
        QuerySpec::Match {
            column,
            terms,
            operator,
            boost,
            fuzziness,
            max_expansions,
            prefix_length,
        } => {
            validate_match_options(*fuzziness, *max_expansions, *prefix_length)?;
            let fields = resolve_match_fields(index, config, column.as_deref())?;
            let mut children = Vec::with_capacity(fields.len());
            let options = FieldMatchOptions {
                operator: *operator,
                boost: *boost,
                fuzziness: *fuzziness,
                max_expansions: *max_expansions,
                prefix_length: *prefix_length,
            };
            for field in fields {
                children.push((
                    Occur::Should,
                    build_field_match_query(index, config, field, terms, options)?,
                ));
            }
            if children.len() == 1 {
                Ok(children.remove(0).1)
            } else {
                Ok(Box::new(BooleanQuery::new(children)))
            }
        }
        QuerySpec::MultiMatch {
            terms,
            columns,
            boosts,
            operator,
            fuzziness,
            max_expansions,
            prefix_length,
        } => {
            validate_multi_match_options(
                columns,
                boosts,
                *fuzziness,
                *max_expansions,
                *prefix_length,
            )?;
            let mut children = Vec::with_capacity(columns.len());
            for (idx, column) in columns.iter().enumerate() {
                let boost = boosts.get(idx).copied().unwrap_or(1.0);
                let field = resolve_text_field(index, config, Some(column))?;
                let options = FieldMatchOptions {
                    operator: *operator,
                    boost,
                    fuzziness: *fuzziness,
                    max_expansions: *max_expansions,
                    prefix_length: *prefix_length,
                };
                children.push((
                    Occur::Should,
                    build_field_match_query(index, config, field, terms, options)?,
                ));
            }
            Ok(Box::new(BooleanQuery::new(children)))
        }
        QuerySpec::MatchPhrase {
            column,
            terms,
            slop,
        } => {
            if !config.tokenizer.with_position {
                return Err(FtIndexError::InvalidQuery(
                    "phrase query requires positions".to_string(),
                ));
            }
            let text_field = resolve_text_field(index, config, column.as_deref())?;
            let parser = QueryParser::for_index(index, vec![text_field]);
            let escaped = terms.replace('\\', "\\\\").replace('"', "\\\"");
            let query_text = if *slop == 0 {
                format!("\"{escaped}\"")
            } else {
                format!("\"{escaped}\"~{slop}")
            };
            parser
                .parse_query(&query_text)
                .map_err(|e| FtIndexError::InvalidQuery(e.to_string()))
        }
        QuerySpec::Boolean {
            should,
            must,
            must_not,
            queries,
        } => {
            if should.is_empty() && must.is_empty() && must_not.is_empty() && queries.is_empty() {
                return Err(FtIndexError::InvalidQuery(
                    "boolean query must contain at least one clause".to_string(),
                ));
            }
            let has_positive_clause = !should.is_empty()
                || !must.is_empty()
                || queries
                    .iter()
                    .any(|(occur, _)| matches!(occur, BooleanOccur::Should | BooleanOccur::Must));
            if !has_positive_clause {
                return Err(FtIndexError::InvalidQuery(
                    "boolean query must contain at least one should or must clause".to_string(),
                ));
            }
            let mut children =
                Vec::with_capacity(should.len() + must.len() + must_not.len() + queries.len());
            for child in should {
                children.push((Occur::Should, build_query(index, config, child)?));
            }
            for child in must {
                children.push((Occur::Must, build_query(index, config, child)?));
            }
            for child in must_not {
                children.push((Occur::MustNot, build_query(index, config, child)?));
            }
            for (occur, child) in queries {
                let occur = match occur {
                    BooleanOccur::Should => Occur::Should,
                    BooleanOccur::Must => Occur::Must,
                    BooleanOccur::MustNot => Occur::MustNot,
                };
                children.push((occur, build_query(index, config, child)?));
            }
            Ok(Box::new(BooleanQuery::new(children)))
        }
        QuerySpec::Boost {
            positive,
            negative,
            negative_boost,
        } => {
            validate_negative_boost(*negative_boost)?;
            Ok(Box::new(DemoteQuery::new(
                build_query(index, config, positive)?,
                build_query(index, config, negative)?,
                *negative_boost,
            )))
        }
    }
}

fn validate_match_options(
    fuzziness: Option<u8>,
    max_expansions: usize,
    prefix_length: u32,
) -> Result<()> {
    if fuzziness.unwrap_or(0) > 2 {
        return Err(FtIndexError::InvalidQuery(
            "match query fuzziness must be auto/null or a value in [0, 2]".to_string(),
        ));
    }
    if max_expansions == 0 {
        return Err(FtIndexError::InvalidQuery(
            "match query max_expansions must be positive".to_string(),
        ));
    }
    let _ = prefix_length;
    Ok(())
}

fn validate_multi_match_options(
    columns: &[String],
    boosts: &[f32],
    fuzziness: Option<u8>,
    max_expansions: usize,
    prefix_length: u32,
) -> Result<()> {
    if columns.is_empty() {
        return Err(FtIndexError::InvalidQuery(
            "multi_match query must contain at least one column".to_string(),
        ));
    }
    if !boosts.is_empty() && boosts.len() != columns.len() {
        return Err(FtIndexError::InvalidQuery(format!(
            "multi_match boosts length {} does not match columns length {}",
            boosts.len(),
            columns.len()
        )));
    }
    for boost in boosts {
        validate_boost(*boost)?;
    }
    validate_match_options(fuzziness, max_expansions, prefix_length)
}

fn validate_boost(boost: f32) -> Result<()> {
    if !boost.is_finite() || boost <= 0.0 {
        return Err(FtIndexError::InvalidQuery(format!(
            "boost must be a finite positive value, got {boost}"
        )));
    }
    Ok(())
}

fn resolve_text_field(
    index: &Index,
    config: &FullTextIndexConfig,
    column: Option<&str>,
) -> Result<tantivy::schema::Field> {
    let column = match column.map(str::trim).filter(|column| !column.is_empty()) {
        Some(column) => column,
        None => {
            let fields = config.indexed_text_fields();
            if fields.len() == 1 {
                fields[0]
            } else {
                return Err(FtIndexError::InvalidQuery(
                    "full-text query column must be set for multi-field indexes".to_string(),
                ));
            }
        }
    };
    if !config.indexed_text_fields().contains(&column) {
        return Err(FtIndexError::InvalidQuery(format!(
            "full-text query column '{column}' is not configured for this index"
        )));
    }
    index
        .schema()
        .get_field(column)
        .map_err(|_| FtIndexError::InvalidQuery(format!("missing text field '{column}'")))
}

fn resolve_match_fields(
    index: &Index,
    config: &FullTextIndexConfig,
    column: Option<&str>,
) -> Result<Vec<tantivy::schema::Field>> {
    if column
        .map(str::trim)
        .filter(|column| !column.is_empty())
        .is_some()
    {
        return Ok(vec![resolve_text_field(index, config, column)?]);
    }
    config
        .indexed_text_fields()
        .into_iter()
        .map(|field| resolve_text_field(index, config, Some(field)))
        .collect()
}

#[derive(Clone, Copy)]
struct FieldMatchOptions {
    operator: MatchOperator,
    boost: f32,
    fuzziness: Option<u8>,
    max_expansions: usize,
    prefix_length: u32,
}

fn build_field_match_query(
    index: &Index,
    config: &FullTextIndexConfig,
    field: tantivy::schema::Field,
    terms: &str,
    options: FieldMatchOptions,
) -> Result<Box<dyn Query>> {
    validate_boost(options.boost)?;
    let tokens = analyze_terms(index, config, terms)?;
    let mut query = if tokens.is_empty() {
        Box::new(EmptyQuery) as Box<dyn Query>
    } else if tokens.len() == 1 {
        build_token_query(
            field,
            &tokens[0],
            options.fuzziness,
            options.max_expansions,
            options.prefix_length,
        )?
    } else {
        let occur = match options.operator {
            MatchOperator::Or => Occur::Should,
            MatchOperator::And => Occur::Must,
        };
        let mut children = Vec::with_capacity(tokens.len());
        for token in tokens {
            children.push((
                occur,
                build_token_query(
                    field,
                    &token,
                    options.fuzziness,
                    options.max_expansions,
                    options.prefix_length,
                )?,
            ));
        }
        Box::new(BooleanQuery::new(children)) as Box<dyn Query>
    };
    if (options.boost - 1.0).abs() > f32::EPSILON {
        query = Box::new(BoostQuery::new(query, options.boost));
    }
    Ok(query)
}

fn analyze_terms(index: &Index, config: &FullTextIndexConfig, terms: &str) -> Result<Vec<String>> {
    let tokenizer_name = tokenizer_name(&config.tokenizer);
    let mut analyzer = index.tokenizers().get(tokenizer_name).ok_or_else(|| {
        FtIndexError::InvalidQuery(format!("tokenizer '{tokenizer_name}' is not registered"))
    })?;
    let mut tokens = Vec::new();
    let mut token_stream = analyzer.token_stream(terms);
    token_stream.process(&mut |token| tokens.push(token.text.clone()));
    Ok(tokens)
}

fn build_token_query(
    field: tantivy::schema::Field,
    token: &str,
    fuzziness: Option<u8>,
    max_expansions: usize,
    prefix_length: u32,
) -> Result<Box<dyn Query>> {
    let fuzziness = fuzziness.unwrap_or_else(|| auto_fuzziness(token));
    if fuzziness == 0 {
        return Ok(Box::new(TermQuery::new(
            Term::from_field_text(field, token),
            IndexRecordOption::WithFreqs,
        )));
    }
    if fuzziness > 2 {
        return Err(FtIndexError::InvalidQuery(
            "match query fuzziness must be auto/null or a value in [0, 2]".to_string(),
        ));
    }
    let term = Term::from_field_text(field, token);
    let prefix = token
        .chars()
        .take(prefix_length as usize)
        .collect::<String>();
    Ok(Box::new(CappedFuzzyTermQuery::new(
        term,
        fuzziness,
        prefix,
        max_expansions,
    )))
}

fn auto_fuzziness(token: &str) -> u8 {
    match token.chars().count() {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}

fn validate_negative_boost(negative_boost: f32) -> Result<()> {
    if !negative_boost.is_finite() || !(0.0..=1.0).contains(&negative_boost) {
        return Err(FtIndexError::InvalidQuery(format!(
            "negative_boost must be a finite value in [0.0, 1.0], got {negative_boost}"
        )));
    }
    Ok(())
}

struct DemoteQuery {
    positive: Box<dyn Query>,
    negative: Box<dyn Query>,
    negative_boost: Score,
}

impl DemoteQuery {
    fn new(positive: Box<dyn Query>, negative: Box<dyn Query>, negative_boost: Score) -> Self {
        Self {
            positive,
            negative,
            negative_boost,
        }
    }
}

impl Clone for DemoteQuery {
    fn clone(&self) -> Self {
        Self {
            positive: self.positive.box_clone(),
            negative: self.negative.box_clone(),
            negative_boost: self.negative_boost,
        }
    }
}

impl fmt::Debug for DemoteQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Demote(positive={:?}, negative={:?}, negative_boost={})",
            self.positive, self.negative, self.negative_boost
        )
    }
}

impl Query for DemoteQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let positive_weight = self.positive.weight(enable_scoring)?;
        if !enable_scoring.is_scoring_enabled() {
            return Ok(positive_weight);
        }
        let negative_weight = self.negative.weight(EnableScoring::Disabled {
            schema: enable_scoring.schema(),
            searcher_opt: enable_scoring.searcher(),
        })?;
        Ok(Box::new(DemoteWeight {
            positive: positive_weight,
            negative: negative_weight,
            negative_boost: self.negative_boost,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a tantivy::Term, bool)) {
        self.positive.query_terms(visitor);
        self.negative.query_terms(visitor);
    }
}

struct DemoteWeight {
    positive: Box<dyn Weight>,
    negative: Box<dyn Weight>,
    negative_boost: Score,
}

impl Weight for DemoteWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(Box::new(DemoteScorer {
            positive: self.positive.scorer(reader, boost)?,
            negative: self.negative.scorer(reader, 1.0)?,
            negative_boost: self.negative_boost,
        }))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let positive_explanation = self.positive.explain(reader, doc)?;
        let mut negative_scorer = self.negative.scorer(reader, 1.0)?;
        let matched_negative = matches_doc(negative_scorer.as_mut(), doc);
        let factor = if matched_negative {
            self.negative_boost
        } else {
            1.0
        };
        let score = positive_explanation.value() * factor;
        let mut explanation =
            Explanation::new_with_string(format!("Demote by negative query x{factor}"), score);
        explanation.add_detail(positive_explanation);
        if matched_negative {
            explanation.add_const("negative_boost", self.negative_boost);
        }
        Ok(explanation)
    }

    fn count(&self, reader: &SegmentReader) -> tantivy::Result<u32> {
        self.positive.count(reader)
    }
}

struct DemoteScorer {
    positive: Box<dyn Scorer>,
    negative: Box<dyn Scorer>,
    negative_boost: Score,
}

impl DocSet for DemoteScorer {
    fn advance(&mut self) -> DocId {
        self.positive.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.positive.seek(target)
    }

    fn doc(&self) -> DocId {
        self.positive.doc()
    }

    fn size_hint(&self) -> u32 {
        self.positive.size_hint()
    }
}

impl Scorer for DemoteScorer {
    fn score(&mut self) -> Score {
        let positive_score = self.positive.score();
        if matches_doc(self.negative.as_mut(), self.positive.doc()) {
            positive_score * self.negative_boost
        } else {
            positive_score
        }
    }
}

fn matches_doc(docset: &mut dyn DocSet, doc: DocId) -> bool {
    if doc == TERMINATED {
        return false;
    }
    let current = docset.doc();
    if current < doc {
        docset.seek(doc) == doc
    } else {
        current == doc
    }
}

#[derive(Clone)]
struct CappedFuzzyTermQuery {
    term: Term,
    distance: u8,
    prefix: String,
    max_expansions: usize,
}

impl CappedFuzzyTermQuery {
    fn new(term: Term, distance: u8, prefix: String, max_expansions: usize) -> Self {
        Self {
            term,
            distance,
            prefix,
            max_expansions,
        }
    }
}

impl fmt::Debug for CappedFuzzyTermQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CappedFuzzyTermQuery(term={:?}, distance={}, prefix={:?}, max_expansions={})",
            self.term, self.distance, self.prefix, self.max_expansions
        )
    }
}

impl Query for CappedFuzzyTermQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let term_value = self.term.value();
        let term_text = term_value.as_str().ok_or_else(|| {
            tantivy::TantivyError::InvalidArgument("fuzzy query requires a string term".to_string())
        })?;
        let builder = LevenshteinAutomatonBuilder::new(self.distance, true);
        let automaton = PrefixedDfaAutomaton::new(
            builder.build_dfa(term_text),
            self.prefix.as_bytes().to_vec(),
        );
        Ok(Box::new(CappedAutomatonWeight {
            field: self.term.field(),
            automaton,
            max_expansions: self.max_expansions,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        visitor(&self.term, false);
    }
}

struct CappedAutomatonWeight<A> {
    field: tantivy::schema::Field,
    automaton: A,
    max_expansions: usize,
}

impl<A> Weight for CappedAutomatonWeight<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let inverted_index = reader.inverted_index(self.field)?;
        let term_dict = inverted_index.terms();
        let mut term_stream = term_dict.search(&self.automaton).into_stream()?;
        let mut docs = Vec::new();
        let mut expansions = 0usize;
        while expansions < self.max_expansions && term_stream.advance() {
            expansions += 1;
            let term_info = term_stream.value();
            let mut block_segment_postings = inverted_index
                .read_block_postings_from_terminfo(term_info, IndexRecordOption::Basic)?;
            loop {
                let block_docs = block_segment_postings.docs();
                if block_docs.is_empty() {
                    break;
                }
                docs.extend_from_slice(block_docs);
                block_segment_postings.advance();
            }
        }
        docs.sort_unstable();
        docs.dedup();
        Ok(Box::new(ConstScorer::new(SortedDocSet::new(docs), boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("CappedAutomatonScorer", 1.0))
        } else {
            Err(tantivy::TantivyError::InvalidArgument(
                "Document does not match fuzzy query".to_string(),
            ))
        }
    }
}

struct SortedDocSet {
    docs: Vec<DocId>,
    cursor: usize,
    doc: DocId,
}

impl SortedDocSet {
    fn new(docs: Vec<DocId>) -> Self {
        let doc = docs.first().copied().unwrap_or(TERMINATED);
        Self {
            docs,
            cursor: 0,
            doc,
        }
    }
}

impl DocSet for SortedDocSet {
    fn advance(&mut self) -> DocId {
        if self.doc == TERMINATED {
            return TERMINATED;
        }
        self.cursor += 1;
        self.doc = self.docs.get(self.cursor).copied().unwrap_or(TERMINATED);
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.doc >= target {
            return self.doc;
        }
        let relative_cursor = self.docs[self.cursor..].partition_point(|doc| *doc < target);
        self.cursor += relative_cursor;
        self.doc = self.docs.get(self.cursor).copied().unwrap_or(TERMINATED);
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        if self.doc == TERMINATED {
            0
        } else {
            (self.docs.len() - self.cursor) as u32
        }
    }
}

struct PrefixedDfaAutomaton {
    dfa: DFA,
    prefix: Vec<u8>,
}

impl PrefixedDfaAutomaton {
    fn new(dfa: DFA, prefix: Vec<u8>) -> Self {
        Self { dfa, prefix }
    }
}

impl Automaton for PrefixedDfaAutomaton {
    type State = (u32, Option<usize>);

    fn start(&self) -> Self::State {
        (self.dfa.initial_state(), Some(0))
    }

    fn is_match(&self, state: &Self::State) -> bool {
        matches!(self.dfa.distance(state.0), Distance::Exact(_))
            && matches!(state.1, Some(pos) if pos >= self.prefix.len())
    }

    fn can_match(&self, state: &Self::State) -> bool {
        state.0 != SINK_STATE && state.1.is_some()
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let dfa_state = self.dfa.transition(state.0, byte);
        let prefix_state = match state.1 {
            None => None,
            Some(pos) if pos >= self.prefix.len() => Some(pos),
            Some(pos) if self.prefix[pos] == byte => Some(pos + 1),
            Some(_) => None,
        };
        (dfa_state, prefix_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_documents_are_not_retained_as_source_strings() -> Result<()> {
        let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;

        for row_id in 0..10_000 {
            writer.add_document(row_id, format!("document {row_id} with unique source text"))?;
        }

        // Keep this exhaustive: the writer state must not grow a source-document collection.
        let FullTextIndexWriter {
            config: _,
            state,
            row_id_field: _,
            text_fields: _,
            document_count,
        } = writer;
        assert!(state.is_some());
        assert_eq!(document_count, 10_000);
        Ok(())
    }
}
