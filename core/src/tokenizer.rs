use crate::error::{FtIndexError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerKind {
    #[default]
    Default,
    Simple,
    Whitespace,
    Raw,
    Ngram,
    Jieba,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenizerConfig {
    pub tokenizer: TokenizerKind,
    pub ngram_min_gram: usize,
    pub ngram_max_gram: usize,
    pub ngram_prefix_only: bool,
    pub jieba_search_mode: bool,
    pub jieba_ordinal_position: bool,
    pub lower_case: bool,
    pub max_token_length: usize,
    pub ascii_folding: bool,
    pub stem: bool,
    pub language: String,
    pub remove_stop_words: bool,
    pub stop_words: Vec<String>,
    pub with_position: bool,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        Self {
            tokenizer: TokenizerKind::Default,
            ngram_min_gram: 2,
            ngram_max_gram: 2,
            ngram_prefix_only: false,
            jieba_search_mode: true,
            jieba_ordinal_position: true,
            lower_case: true,
            max_token_length: 40,
            ascii_folding: false,
            stem: false,
            language: "english".to_string(),
            remove_stop_words: false,
            stop_words: Vec::new(),
            with_position: true,
        }
    }
}

impl TokenizerConfig {
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self> {
        let mut config = Self::default();
        for (raw_key, value) in options {
            let key = raw_key
                .strip_prefix("fulltext.")
                .or_else(|| raw_key.strip_prefix("tantivy."))
                .unwrap_or(raw_key.as_str());
            match key {
                "tokenizer" => {
                    config.tokenizer = parse_tokenizer(value)?;
                }
                "ngram.min-gram" => {
                    config.ngram_min_gram = parse_usize(key, value)?;
                }
                "ngram.max-gram" => {
                    config.ngram_max_gram = parse_usize(key, value)?;
                }
                "ngram.prefix-only" => {
                    config.ngram_prefix_only = parse_bool(key, value)?;
                }
                "jieba.search-mode" => {
                    config.jieba_search_mode = parse_bool(key, value)?;
                }
                "jieba.ordinal-position" | "jieba.ordinal-position-mode" => {
                    config.jieba_ordinal_position = parse_bool(key, value)?;
                }
                "lower-case" => {
                    config.lower_case = parse_bool(key, value)?;
                }
                "max-token-length" => {
                    config.max_token_length = parse_usize(key, value)?;
                }
                "ascii-folding" => {
                    config.ascii_folding = parse_bool(key, value)?;
                }
                "stem" => {
                    config.stem = parse_bool(key, value)?;
                }
                "language" => {
                    config.language = value.trim().to_lowercase();
                }
                "remove-stop-words" => {
                    config.remove_stop_words = parse_bool(key, value)?;
                }
                "stop-words" => {
                    config.stop_words = value
                        .split(';')
                        .map(str::trim)
                        .filter(|word| !word.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                "with-position" => {
                    config.with_position = parse_bool(key, value)?;
                }
                _ => {}
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.ngram_min_gram == 0 {
            return invalid("ngram.min-gram", "must be positive");
        }
        if self.ngram_max_gram == 0 {
            return invalid("ngram.max-gram", "must be positive");
        }
        if self.ngram_min_gram > self.ngram_max_gram {
            return invalid("ngram.min-gram", "must not exceed ngram.max-gram");
        }
        if self.max_token_length == 0 {
            return invalid("max-token-length", "must be positive");
        }
        Ok(())
    }
}

fn parse_tokenizer(value: &str) -> Result<TokenizerKind> {
    match value.trim().to_lowercase().as_str() {
        "default" => Ok(TokenizerKind::Default),
        "simple" => Ok(TokenizerKind::Simple),
        "whitespace" => Ok(TokenizerKind::Whitespace),
        "raw" => Ok(TokenizerKind::Raw),
        "ngram" => Ok(TokenizerKind::Ngram),
        "jieba" => Ok(TokenizerKind::Jieba),
        other => Err(FtIndexError::InvalidOption {
            key: "tokenizer".to_string(),
            message: format!("unsupported tokenizer '{other}'"),
        }),
    }
}

fn parse_usize(key: &str, value: &str) -> Result<usize> {
    value
        .trim()
        .parse()
        .map_err(|_| FtIndexError::InvalidOption {
            key: key.to_string(),
            message: format!("expected unsigned integer, got '{value}'"),
        })
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(FtIndexError::InvalidOption {
            key: key.to_string(),
            message: format!("expected boolean, got '{value}'"),
        }),
    }
}

fn invalid<T>(key: &str, message: &str) -> Result<T> {
    Err(FtIndexError::InvalidOption {
        key: key.to_string(),
        message: message.to_string(),
    })
}
