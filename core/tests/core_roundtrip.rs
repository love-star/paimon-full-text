use paimon_ftindex_core::io::{PosWriter, SliceReader};
use paimon_ftindex_core::{
    FullTextIndexConfig, FullTextIndexReader, FullTextIndexWriter, FullTextQuery, MatchOperator,
    TokenizerConfig, TokenizerKind,
};
use roaring::RoaringTreemap;
use std::collections::HashMap;

fn build_index() -> anyhow::Result<Vec<u8>> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
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

#[test]
fn match_query_round_trip() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(FullTextQuery::match_query("paimon", "text"), 10)?;

    assert_eq!(reader.metadata().document_count, 3);
    assert_eq!(result.row_ids.len(), 2);
    assert!(result.row_ids.contains(&10));
    assert!(result.row_ids.contains(&12));
    assert_eq!(result.scores.len(), 2);
    Ok(())
}

#[test]
fn match_query_and_operator_filters_terms() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = FullTextQuery::Match {
        column: "text".to_string(),
        terms: "paimon indexes".to_string(),
        operator: MatchOperator::And,
        boost: 1.0,
    };
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![12]);
    Ok(())
}

#[test]
fn search_with_roaring_filter_limits_allowed_row_ids_before_top_docs() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let query = FullTextQuery::match_query("paimon", "text");

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes.clone()))?;
    let unfiltered_top = reader.search(query.clone(), 1)?.row_ids[0];
    let allowed_id = if unfiltered_top == 10 { 12 } else { 10 };

    let mut allowed = RoaringTreemap::new();
    allowed.insert(allowed_id as u64);
    let mut filter_bytes = Vec::new();
    allowed.serialize_into(&mut filter_bytes)?;

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
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

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search_with_roaring_filter(
        FullTextQuery::match_query("paimon", "text"),
        10,
        &filter_bytes,
    )?;

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

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search_with_roaring_filter(
        FullTextQuery::match_query("paimon", "text"),
        10,
        &filter_bytes,
    )?;

    assert_eq!(result.row_ids, vec![allowed_id]);
    Ok(())
}

#[test]
fn search_rejects_invalid_roaring_filter_bytes() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let err = reader
        .search_with_roaring_filter(
            FullTextQuery::match_query("paimon", "text"),
            10,
            b"not roaring",
        )
        .expect_err("invalid filter bytes should fail");

    assert!(err.to_string().contains("invalid RoaringTreemap filter"));
    Ok(())
}

#[test]
fn phrase_query_uses_positions() -> anyhow::Result<()> {
    let bytes = build_index()?;
    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(FullTextQuery::phrase("full text", "text"), 10)?;

    assert_eq!(result.row_ids, vec![10]);
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

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(FullTextQuery::match_query("中华", "text"), 10)?;

    assert_eq!(result.row_ids, vec![20]);
    Ok(())
}

#[test]
fn jieba_tokenizer_supports_chinese_phrase_queries() -> anyhow::Result<()> {
    let config = FullTextIndexConfig::new().tokenizer(TokenizerConfig {
        tokenizer: TokenizerKind::Jieba,
        jieba_ordinal_position: true,
        ..TokenizerConfig::default()
    });
    let mut writer = FullTextIndexWriter::new(config)?;
    writer.add_document(30, "北京大学支持全文检索")?;
    writer.add_document(31, "北京的大学很多")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let result = reader.search(FullTextQuery::phrase("北京大学", "text"), 10)?;

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
fn boost_query_requires_positive_match() -> anyhow::Result<()> {
    let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
    writer.add_document(1, "apache paimon")?;
    writer.add_document(2, "tantivy only")?;

    let mut bytes = Vec::new();
    writer.write(&mut PosWriter::new(&mut bytes))?;

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = FullTextQuery::Boost {
        positive: Box::new(FullTextQuery::match_query("paimon", "text")),
        negative: Box::new(FullTextQuery::match_query("tantivy", "text")),
        negative_boost: 0.5,
    };
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

    let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
    let query = FullTextQuery::Boost {
        positive: Box::new(FullTextQuery::match_query("paimon", "text")),
        negative: Box::new(FullTextQuery::match_query("bad", "text")),
        negative_boost: 0.5,
    };
    let result = reader.search(query, 10)?;

    assert_eq!(result.row_ids, vec![1, 2]);
    assert!(result.scores[0] > result.scores[1]);
    Ok(())
}

#[test]
fn query_json_round_trip() -> anyhow::Result<()> {
    let query = FullTextQuery::match_query("apache paimon", "text").operator_and();
    let json = query.to_json()?;
    let parsed = FullTextQuery::from_json(&json)?;

    assert_eq!(parsed, query);
    Ok(())
}
