# Paimon Full Text Index Design

## Problem Statement

We want to split Paimon's Tantivy-based full-text search implementation into a
standalone repository, shaped like `apache/paimon-vector-index`, and expose a
consistent Rust, Java, and Python API.

The upstream `paimon-tantivy` code is still pre-production for this use case, so
the standalone project does not need to read or write its existing archive
format. The new repository should use the cleanest v1 format and API.

## Chosen Approach

Use a Rust-first architecture:

- `core`: owns Tantivy schema, tokenizer config, query semantics, storage
  format, writer, reader, and tests.
- `ffi`: exposes a stable C ABI over `core`.
- `jni`: implements Java native calls by delegating to the Rust core.
- `java`: provides a small public Java API using Paimon-style input/output
  callbacks.
- `python`: provides a ctypes binding over the C ABI.

This matches the proven shape of `paimon-vector-index` and avoids divergent
Java and Python implementations.

## Repository Layout

```text
paimon-full-text/
  Cargo.toml
  core/
    Cargo.toml
    src/
      config.rs
      document.rs
      error.rs
      index.rs
      io.rs
      query.rs
      tokenizer.rs
      storage.rs
      lib.rs
    tests/
  ffi/
    Cargo.toml
    build.rs
    cbindgen.toml
    src/lib.rs
  jni/
    Cargo.toml
    src/lib.rs
  include/
    paimon_ftindex.h
    paimon_ftindex.hpp
  java/
    pom.xml
    src/main/java/org/apache/paimon/index/fulltext/
  python/
    pyproject.toml
    setup.py
    paimon_ftindex/
  c/
    CMakeLists.txt
    test_ftindex.c
  cpp/
    CMakeLists.txt
    test_ftindex.cpp
  docs/
    storage-format.md
    java-api.md
    python-api.md
    paimon-integration.md
```

## Storage Format

The v1 index file is self-describing. Readers must be able to open a file
without Paimon manifest metadata.

```text
magic:      8 bytes  "PFTIDX01"
version:    u32      1
header_len: u32
header:     JSON metadata and archive directory
body:       concatenated Tantivy files
checksum:   optional, future v2
```

Header JSON:

```json
{
  "format_version": 1,
  "metadata": {
    "format_version": 1,
    "config": {
      "row_id_field": "row_id",
      "text_field": "text",
      "tokenizer": {
        "tokenizer": "default",
        "lower_case": true,
        "max_token_length": 40,
        "with_position": true
      }
    },
    "document_count": 0,
    "tantivy_version": "0.26.1"
  },
  "files": [
    {"name": "meta.json", "offset": 0, "length": 1234}
  ]
}
```

File offsets are relative to the body start. The archive directory lives in the
front header so C, Java, and Python readers only need positional `read_at`
callbacks and do not need to know total file length.

## Rust API

Rust is the source of truth.

```rust
use paimon_ftindex_core::{
    FullTextIndexConfig, FullTextIndexReader, FullTextIndexWriter,
    FullTextQuery, TokenizerConfig,
};

let config = FullTextIndexConfig::new()
    .tokenizer(TokenizerConfig::default())
    .with_positions(true);

let mut writer = FullTextIndexWriter::new(config)?;
writer.add_document(1, "Apache Paimon supports full text search")?;
writer.write(&mut output)?;

let mut reader = FullTextIndexReader::open(input)?;
reader.optimize_for_search()?;
let result = reader.search(
    FullTextQuery::match_query("paimon", "text").operator_or(),
    10,
)?;
```

Core concepts:

- `FullTextIndexConfig`: schema and tokenizer config.
- `TokenizerConfig`: `default`, `simple`, `whitespace`, `raw`, `ngram`,
  `jieba`, filters, stop words, stemming, positions.
- `FullTextDocument`: row id plus named text fields.
- `FullTextQuery`: structured query DSL.
- `FullTextSearchResult`: row ids plus BM25 scores.
- `SeekRead` and `SeekWrite`: callback-based I/O like vector-index.

First release supports one logical text field named `text`. The file format
already stores field names so multi-field search can be added without changing
the public storage envelope.

## Query DSL

Keep the Paimon/LanceDB-style structured JSON, but implement parsing in Rust.

Supported v1 queries:

- `match`: tokenized term query with `OR` or `AND`.
- `match_phrase`: phrase query when positions are enabled.
- `boolean`: `should`, `must`, `must_not`.
- `boost`: positive query must match; matching the negative query multiplies
  the positive score by `negative_boost` for demotion.

Deferred:

- `multi_match`: depends on multi-field index support.
- arbitrary Tantivy query strings: useful for debugging but too easy to make
  unstable as a public API.

Example:

```json
{
  "match": {
    "column": "text",
    "terms": "apache paimon",
    "operator": "And",
    "boost": 1.0
  }
}
```

## C ABI

The C ABI is the stable interop boundary.

Naming:

- library: `libpaimon_ftindex_ffi`
- header: `include/paimon_ftindex.h`
- prefix: `paimon_ftindex_`

Main handles:

- `PaimonFtindexConfigHandle`
- `PaimonFtindexWriterHandle`
- `PaimonFtindexReaderHandle`
- `PaimonFtindexSearchResult`

I/O callbacks:

```c
typedef struct {
    void *ctx;
    int (*write_fn)(void *ctx, const uint8_t *buf, size_t len);
    int (*flush_fn)(void *ctx);
    int64_t (*get_pos_fn)(void *ctx);
} PaimonFtindexOutputFile;

typedef struct {
    void *ctx;
    int (*read_at_fn)(void *ctx, uint64_t pos, uint8_t *buf, size_t len);
} PaimonFtindexInputFile;
```

Core calls:

```c
PaimonFtindexWriterHandle *paimon_ftindex_writer_open(
    const char **keys,
    const char **values,
    size_t len);

int paimon_ftindex_writer_add_document(
    PaimonFtindexWriterHandle *writer,
    int64_t row_id,
    const char *text);

int paimon_ftindex_writer_write_index(
    PaimonFtindexWriterHandle *writer,
    PaimonFtindexOutputFile output);

PaimonFtindexReaderHandle *paimon_ftindex_reader_open(
    PaimonFtindexInputFile input);

int paimon_ftindex_reader_search_json(
    PaimonFtindexReaderHandle *reader,
    const char *query_json,
    size_t limit,
    int64_t *row_ids,
    float *scores,
    size_t capacity,
    size_t *result_len);

int paimon_ftindex_reader_search_json_with_roaring_filter(
    PaimonFtindexReaderHandle *reader,
    const char *query_json,
    size_t limit,
    const uint8_t *roaring_filter,
    size_t roaring_filter_len,
    int64_t *row_ids,
    float *scores,
    size_t capacity,
    size_t *result_len);
```

All functions return `0` on success and `-1` on error. A thread-local
`paimon_ftindex_last_error()` exposes the error message. Native panics are
caught at the FFI boundary.

## Java API

Package:

```text
org.apache.paimon.index.fulltext
```

Public classes:

- `FullTextIndexInput`
- `FullTextIndexOutput`
- `FullTextIndexWriter`
- `FullTextIndexReader`
- `FullTextIndexMetadata`
- `FullTextQuery`
- `FullTextSearchResult`
- `FullTextNative`

Usage:

```java
Map<String, String> options = new HashMap<>();
options.put("tokenizer", "ngram");
options.put("ngram.min-gram", "2");
options.put("ngram.max-gram", "2");

try (FullTextIndexWriter writer = FullTextIndexWriter.create(options)) {
    writer.addDocument(1L, "Apache Paimon supports full text search");
    writer.writeIndex(output);
}

try (FullTextIndexReader reader = new FullTextIndexReader(input)) {
    FullTextSearchResult result =
            reader.search(FullTextQuery.match("paimon", "text"), 10);
    FullTextSearchResult filtered =
            reader.search(FullTextQuery.match("paimon", "text"), 10, roaringFilterBytes);
}
```

Java should not expose Tantivy classes. It should use string options so Paimon
table and procedure options map directly into the standalone library.

## Python API

Package:

```text
paimon_ftindex
```

Python uses ctypes over the C ABI and mirrors Java names in Python style.

```python
from paimon_ftindex import FullTextIndexReader, FullTextIndexWriter, MatchQuery

writer = FullTextIndexWriter({
    "tokenizer": "ngram",
    "ngram.min-gram": "2",
    "ngram.max-gram": "2",
})
writer.add_document(1, "Apache Paimon supports full text search")
writer.write(output)

reader = FullTextIndexReader(input)
ids, scores = reader.search(MatchQuery("paimon", column="text"), limit=10)
filtered_ids, filtered_scores = reader.search(
    MatchQuery("paimon", column="text"),
    limit=10,
    filter_bytes=roaring_filter_bytes,
)
```

Python I/O protocol:

- output object: `write(bytes)`, optional `flush()`, optional `tell()`.
- input object: `pread(pos: int, length: int) -> bytes`.

The Python package should not depend on `tantivy-py`; Rust core provides the
search semantics.

## Paimon Integration

The Paimon core repository should eventually keep only a thin integration
module:

- `paimon-fulltext` or `paimon-full-text`
- `GlobalIndexerFactory` identifier: `tantivy-fulltext` or `fulltext`
- index writer delegates to `FullTextIndexWriter`
- index reader delegates to `FullTextIndexReader`
- `FullTextQuery` and `FullTextSearch` in `paimon-common` can stay as the query
  API and serialize to the JSON understood by this library

Recommended Paimon option namespace:

```text
fulltext.tokenizer
fulltext.ngram.min-gram
fulltext.ngram.max-gram
fulltext.ngram.prefix-only
fulltext.jieba.search-mode
fulltext.jieba.ordinal-position
fulltext.lower-case
fulltext.max-token-length
fulltext.ascii-folding
fulltext.stem
fulltext.language
fulltext.remove-stop-words
fulltext.stop-words
fulltext.with-position
```

The standalone library should accept both unprefixed keys and `fulltext.`
prefixed keys for caller convenience.

## Tokenizer Policy

Supported first implementation tokenizers:

- `default`
- `simple`
- `whitespace`
- `raw`
- `ngram`
- `jieba`

Reserved follow-up tokenizers and filters:

- stemming
- built-in stop words
- custom stop words

Supported first implementation filters:

- lowercase
- max token length
- ASCII folding

The `jieba` tokenizer uses Tantivy's current external tokenizer API via
`tantivy-jieba`. Search mode and ordinal token positions are enabled by
default for Chinese full-text recall and phrase query correctness.

No dynamic Rust tokenizer plugins in v1. They complicate packaging and native
loading too much for an initial library.

## Testing Strategy

Core tests:

- storage format round trip
- metadata round trip
- query JSON parsing
- tokenizer config validation
- match, phrase, boolean, boost query behavior
- seek-based input reads only requested byte ranges where possible

Cross-language tests:

- Rust writes, Java reads
- Java writes, Rust reads
- Rust writes, Python reads
- Python writes, Rust reads
- Java and Python return the same row ids and scores for fixed fixtures

Fixture policy:

- Commit small hex fixtures for v1 format.
- Add a test that fails on accidental format drift unless the version changes.

## Release Strategy

Publish artifacts independently:

- Rust crates: `paimon-ftindex-core`, `paimon-ftindex-ffi`,
  `paimon-ftindex-jni`.
- Java artifact: `org.apache.paimon:paimon-full-text-index`.
- Python package: `paimon-ftindex`.

Linux native binaries should be built on the oldest supported glibc baseline,
matching the caution already present in upstream `paimon-tantivy-jni`.

## Out Of Scope

- Reading old upstream `paimon-tantivy` archive files.
- Multi-field indexing in the first release.
- Highlighting/snippets.
- Arbitrary query-string API as a stable surface.
- Distributed index merge and compaction policy inside this repository.
- Paimon SQL procedures and optimizer rules. Those remain Paimon repository
  integration work.

## Open Questions

- Should the public index type be `fulltext` or keep `tantivy-fulltext` for
  clarity?
- Should Java native loading copy prebuilt libraries from resources, or should
  this repository initially require `PAIMON_FTINDEX_LIB_PATH` like the Python
  package?
