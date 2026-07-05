use crate::config::{FullTextIndexConfig, FullTextIndexMetadata};
use crate::error::{FtIndexError, Result};
use crate::io::{SeekRead, SeekWrite};
use crate::query::{BooleanOccur, FullTextQuery, MatchOperator};
use crate::storage::{read_exact_at, read_header, write_envelope, ArchiveFileEntry, IndexHeader};
use crate::tokenizer::{TokenizerConfig, TokenizerKind};
use roaring::RoaringTreemap;
use std::fmt;
use std::fs;
use std::path::Path;
use tantivy::collector::{FilterCollector, TopDocs};
use tantivy::directory::{Directory, RamDirectory};
use tantivy::query::{
    BooleanQuery, BoostQuery, EnableScoring, Explanation, Occur, Query, QueryParser, Scorer, Weight,
};
use tantivy::schema::{IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter,
    SimpleTokenizer, TextAnalyzer, WhitespaceTokenizer,
};
use tantivy::{DocId, DocSet, Index, Score, SegmentReader, TantivyDocument, TERMINATED};
use tantivy_jieba::JiebaTokenizer;
use tempfile::TempDir;

#[derive(Clone, Debug, PartialEq)]
pub struct FullTextSearchResult {
    pub row_ids: Vec<i64>,
    pub scores: Vec<f32>,
}

pub struct FullTextIndexWriter {
    config: FullTextIndexConfig,
    documents: Vec<(i64, String)>,
}

impl FullTextIndexWriter {
    pub fn new(config: FullTextIndexConfig) -> Result<Self> {
        config.tokenizer.validate()?;
        Ok(Self {
            config,
            documents: Vec::new(),
        })
    }

    pub fn add_document(&mut self, row_id: i64, text: impl Into<String>) -> Result<()> {
        if row_id < 0 {
            return Err(FtIndexError::InvalidStorage(format!(
                "row id must be non-negative, got {row_id}"
            )));
        }
        self.documents.push((row_id, text.into()));
        Ok(())
    }

    pub fn write<W: SeekWrite>(&mut self, output: &mut W) -> Result<()> {
        let temp_dir = TempDir::new()?;
        let schema = build_schema(&self.config);
        let mut index = Index::create_in_dir(temp_dir.path(), schema.clone())?;
        register_tokenizer(&mut index, &self.config.tokenizer)?;
        let row_id_field = schema
            .get_field(&self.config.row_id_field)
            .map_err(|_| FtIndexError::InvalidStorage("missing row_id field".to_string()))?;
        let text_field = schema
            .get_field(&self.config.text_field)
            .map_err(|_| FtIndexError::InvalidStorage("missing text field".to_string()))?;

        {
            let mut index_writer = index.writer(50_000_000)?;
            for (row_id, text) in &self.documents {
                let mut doc = TantivyDocument::new();
                doc.add_u64(row_id_field, *row_id as u64);
                doc.add_text(text_field, text);
                index_writer.add_document(doc)?;
            }
            index_writer.commit()?;
        }

        let files = collect_index_files(temp_dir.path())?;
        let mut offset = 0u64;
        let mut entries = Vec::with_capacity(files.len());
        for (name, data) in &files {
            entries.push(ArchiveFileEntry {
                name: name.clone(),
                offset,
                length: data.len() as u64,
            });
            offset += data.len() as u64;
        }

        let header = IndexHeader {
            metadata: FullTextIndexMetadata {
                format_version: crate::storage::FORMAT_VERSION,
                config: self.config.clone(),
                document_count: self.documents.len() as u64,
                tantivy_version: tantivy::version().to_string(),
            },
            files: entries,
        };
        write_envelope(output, &header, &files)
    }
}

pub struct FullTextIndexReader<R> {
    _input: R,
    index: Index,
    metadata: FullTextIndexMetadata,
}

impl<R: SeekRead> FullTextIndexReader<R> {
    pub fn open(mut input: R) -> Result<Self> {
        let (header, body_start) = read_header(&mut input)?;
        let directory = RamDirectory::create();
        for file in &header.files {
            let mut data = vec![0u8; file.length as usize];
            read_exact_at(&mut input, body_start + file.offset, &mut data)?;
            directory.atomic_write(Path::new(&file.name), &data)?;
        }
        let mut index = Index::open(directory)?;
        register_tokenizer(&mut index, &header.metadata.config.tokenizer)?;
        Ok(Self {
            _input: input,
            index,
            metadata: header.metadata,
        })
    }

    pub fn optimize_for_search(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn metadata(&self) -> &FullTextIndexMetadata {
        &self.metadata
    }

    pub fn search(&mut self, query: FullTextQuery, limit: usize) -> Result<FullTextSearchResult> {
        self.search_with_filter(query, limit, None)
    }

    pub fn search_with_roaring_filter(
        &mut self,
        query: FullTextQuery,
        limit: usize,
        roaring_filter_bytes: &[u8],
    ) -> Result<FullTextSearchResult> {
        let filter = decode_roaring_filter(roaring_filter_bytes)?;
        self.search_with_filter(query, limit, Some(filter))
    }

    fn search_with_filter(
        &mut self,
        query: FullTextQuery,
        limit: usize,
        filter: Option<RoaringTreemap>,
    ) -> Result<FullTextSearchResult> {
        if limit == 0 {
            return Err(FtIndexError::InvalidQuery(
                "search limit must be positive".to_string(),
            ));
        }
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
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
    builder.add_text_field(
        &config.text_field,
        TextOptions::default().set_indexing_options(indexing),
    );
    builder.build()
}

fn tokenizer_name(config: &TokenizerConfig) -> &'static str {
    match config.tokenizer {
        TokenizerKind::Default if !needs_custom_default(config) => "default",
        TokenizerKind::Default | TokenizerKind::Simple => "paimon_custom",
        TokenizerKind::Whitespace => "paimon_custom",
        TokenizerKind::Raw => "paimon_custom",
        TokenizerKind::Ngram => "paimon_ngram",
        TokenizerKind::Jieba => "paimon_jieba",
    }
}

fn needs_custom_default(config: &TokenizerConfig) -> bool {
    !config.lower_case
        || config.max_token_length != 40
        || config.ascii_folding
        || config.stem
        || config.remove_stop_words
        || !config.stop_words.is_empty()
}

fn register_tokenizer(index: &mut Index, config: &TokenizerConfig) -> Result<()> {
    match config.tokenizer {
        TokenizerKind::Default if !needs_custom_default(config) => Ok(()),
        _ => {
            let analyzer = build_text_analyzer(config)?;
            index
                .tokenizers()
                .register(tokenizer_name(config), analyzer);
            Ok(())
        }
    }
}

fn build_text_analyzer(config: &TokenizerConfig) -> Result<TextAnalyzer> {
    if config.stem || config.remove_stop_words || !config.stop_words.is_empty() {
        return Err(FtIndexError::InvalidOption {
            key: "tokenizer filters".to_string(),
            message: "stemming and stop-word filters are not enabled in this first implementation"
                .to_string(),
        });
    }
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
    if config.ascii_folding {
        builder = builder.filter_dynamic(AsciiFoldingFilter);
    }
    Ok(builder.build())
}

fn collect_index_files(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
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
        files.push((name, fs::read(path)?));
    }
    Ok(files)
}

fn build_query(
    index: &Index,
    config: &FullTextIndexConfig,
    query: &FullTextQuery,
) -> Result<Box<dyn Query>> {
    match query {
        FullTextQuery::Match {
            column,
            terms,
            operator,
            boost,
        } => {
            validate_column(config, column)?;
            let text_field = index
                .schema()
                .get_field(&config.text_field)
                .map_err(|_| FtIndexError::InvalidQuery("missing text field".to_string()))?;
            let mut parser = QueryParser::for_index(index, vec![text_field]);
            if *operator == MatchOperator::And {
                parser.set_conjunction_by_default();
            }
            let parsed = parser
                .parse_query(terms)
                .map_err(|e| FtIndexError::InvalidQuery(e.to_string()))?;
            if (*boost - 1.0).abs() > f32::EPSILON {
                Ok(Box::new(BoostQuery::new(parsed, *boost)))
            } else {
                Ok(parsed)
            }
        }
        FullTextQuery::MatchPhrase {
            column,
            terms,
            slop,
        } => {
            validate_column(config, column)?;
            if !config.tokenizer.with_position {
                return Err(FtIndexError::InvalidQuery(
                    "phrase query requires positions".to_string(),
                ));
            }
            let text_field = index
                .schema()
                .get_field(&config.text_field)
                .map_err(|_| FtIndexError::InvalidQuery("missing text field".to_string()))?;
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
        FullTextQuery::Boolean { queries } => {
            if queries.is_empty() {
                return Err(FtIndexError::InvalidQuery(
                    "boolean query must contain at least one clause".to_string(),
                ));
            }
            let mut children = Vec::with_capacity(queries.len());
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
        FullTextQuery::Boost {
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

fn validate_column(config: &FullTextIndexConfig, column: &str) -> Result<()> {
    if column == config.text_field {
        Ok(())
    } else {
        Err(FtIndexError::InvalidQuery(format!(
            "unknown full-text column '{column}'"
        )))
    }
}
