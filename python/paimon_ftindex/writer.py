# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

import ctypes
from ctypes import c_char_p, c_void_p

from ._ffi import (
    FLUSH_FN,
    WRITE_FN,
    PaimonFtindexOutputFile,
    check_ptr,
    check_status,
    lib,
)


class FullTextIndexWriter:
    def __init__(self, options=None):
        options = options or {}
        self._closed = False
        self._output_refs = None
        keys = [str(k).encode("utf-8") for k in options.keys()]
        values = [str(v).encode("utf-8") for v in options.values()]
        key_array = (c_char_p * len(keys))(*keys) if keys else None
        value_array = (c_char_p * len(values))(*values) if values else None
        self._ptr = check_ptr(
            lib.paimon_ftindex_writer_open(key_array, value_array, len(keys))
        )

    def add_document(self, row_id, text, column=None):
        if self._closed:
            raise RuntimeError("FullTextIndexWriter is closed")
        if isinstance(text, dict):
            if column is not None:
                raise ValueError("column must not be set when text is a dict")
            self.add_document_fields(row_id, text)
            return
        if column is not None:
            self.add_document_fields(row_id, [(column, text)])
            return
        check_status(
            lib.paimon_ftindex_writer_add_document(
                self._ptr, int(row_id), str(text).encode("utf-8")
            )
        )

    def add_document_fields(self, row_id, fields):
        if self._closed:
            raise RuntimeError("FullTextIndexWriter is closed")
        items = list(fields.items()) if hasattr(fields, "items") else list(fields)
        if not items:
            raise ValueError("document fields must not be empty")
        names = [str(name).encode("utf-8") for name, _ in items]
        texts = [str(text).encode("utf-8") for _, text in items]
        name_array = (c_char_p * len(names))(*names)
        text_array = (c_char_p * len(texts))(*texts)
        check_status(
            lib.paimon_ftindex_writer_add_document_fields(
                self._ptr, int(row_id), name_array, text_array, len(items)
            )
        )

    def write(self, output):
        """Finalize this writer and stream the index archive to ``output``.

        Every write attempt finalizes the native writer, even when writing or flushing fails.
        Discard a potentially partial output and create a new writer to retry.
        """
        if self._closed:
            raise RuntimeError("FullTextIndexWriter is closed")

        @WRITE_FN
        def write_fn(ctx, buf, length):
            try:
                data = ctypes.string_at(buf, length)
                output.write(data)
                return 0
            except Exception:
                return -1

        @FLUSH_FN
        def flush_fn(ctx):
            try:
                flush = getattr(output, "flush", None)
                if flush is not None:
                    flush()
                return 0
            except Exception:
                return -1

        self._output_refs = (write_fn, flush_fn)
        native_output = PaimonFtindexOutputFile(
            c_void_p(0),
            write_fn,
            flush_fn,
        )
        check_status(lib.paimon_ftindex_writer_write_index(self._ptr, native_output))

    def close(self):
        if not self._closed:
            self._closed = True
            if self._ptr:
                lib.paimon_ftindex_writer_free(self._ptr)
                self._ptr = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()

    def __del__(self):
        self.close()
