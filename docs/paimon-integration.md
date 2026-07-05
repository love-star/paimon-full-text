# Paimon Integration

The standalone library is intentionally independent of Paimon core. Paimon
integration should be a thin adapter:

- Keep `FullTextQuery` / `FullTextSearch` in Paimon common as query API.
- Serialize queries to the JSON accepted by this library.
- Implement a Paimon `GlobalIndexerFactory` that delegates to Java
  `FullTextIndexWriter` and `FullTextIndexReader`.
- Pass serialized 64-bit Roaring row-id filters to reader search when another
  index or predicate pushdown has already produced an allowed candidate set.
- Store produced files as global index files.

Suggested index identifier:

```text
fulltext
```

Suggested option namespace:

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
fulltext.with-position
```

The standalone library accepts both unprefixed keys and `fulltext.` prefixed
keys.
