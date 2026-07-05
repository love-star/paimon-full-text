# Apache Paimon Full Text Index

Standalone Tantivy-based full-text index library for Apache Paimon-style data
lake storage. The project follows the same shape as `paimon-vector-index`:

- `core`: Rust implementation and v1 storage format.
- `ffi`: C ABI over the Rust core.
- `jni`: Java JNI bridge over the Rust core.
- `java`: public Java API.
- `python`: Python ctypes API over the C ABI.

The index file is self-describing. Readers only need positional `read_at` I/O
and do not depend on Paimon manifest metadata.

## Current Status

Implemented:

- Rust writer, reader, v1 envelope, and search.
- C FFI writer/reader/search JSON, including serialized 64-bit Roaring row-id
  filters.
- Java API and JNI bridge.
- Python ctypes package.
- Cross-boundary round-trip tests for Rust core, FFI, Java/JNI, and Python.

Supported tokenizers in this first implementation:

- `default`
- `simple`
- `whitespace`
- `raw`
- `ngram`
- `jieba`

Reserved for follow-up:

- stemming
- built-in and custom stop-word filters
- true seek-on-demand Tantivy directory instead of loading segment files into
  memory at reader open

## Build

```bash
cargo test -p paimon-ftindex-core
cargo test -p paimon-ftindex-ffi
cargo build -p paimon-ftindex-ffi
cargo build -p paimon-ftindex-jni
mvn -q -f java/pom.xml test
PYTHONPATH=python python3 -m pytest -q python/tests
```

## Rust Example

```rust
use paimon_ftindex_core::io::{PosWriter, SliceReader};
use paimon_ftindex_core::{
    FullTextIndexConfig, FullTextIndexReader, FullTextIndexWriter, FullTextQuery,
};

let mut writer = FullTextIndexWriter::new(FullTextIndexConfig::new())?;
writer.add_document(1, "Apache Paimon full text search")?;

let mut bytes = Vec::new();
writer.write(&mut PosWriter::new(&mut bytes))?;

let mut reader = FullTextIndexReader::open(SliceReader::new(bytes))?;
let result = reader.search(FullTextQuery::match_query("paimon", "text"), 10)?;
```

To restrict search to an upstream candidate set, pass a serialized
`RoaringTreemap` of allowed row ids:

```rust
let filtered = reader.search_with_roaring_filter(
    FullTextQuery::match_query("paimon", "text"),
    10,
    roaring_filter_bytes,
)?;
```

## Python Example

```python
from io import BytesIO
from paimon_ftindex import FullTextIndexReader, FullTextIndexWriter, MatchQuery

out = BytesIO()
with FullTextIndexWriter() as writer:
    writer.add_document(1, "Apache Paimon full text search")
    writer.write(out)

class Input:
    def __init__(self, data):
        self.data = data
    def pread(self, pos, length):
        return self.data[pos:pos + length]

with FullTextIndexReader(Input(out.getvalue())) as reader:
    ids, scores = reader.search(MatchQuery("paimon"), limit=10)
    filtered_ids, filtered_scores = reader.search(
        MatchQuery("paimon"), limit=10, filter_bytes=roaring_filter_bytes
    )
```
