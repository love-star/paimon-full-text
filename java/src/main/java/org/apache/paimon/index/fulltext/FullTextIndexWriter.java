/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

package org.apache.paimon.index.fulltext;

import java.util.Map;

public final class FullTextIndexWriter implements AutoCloseable {

    private long nativePtr;

    private FullTextIndexWriter(long nativePtr) {
        this.nativePtr = nativePtr;
    }

    public static FullTextIndexWriter create(Map<String, String> options) {
        String[] keys = new String[options == null ? 0 : options.size()];
        String[] values = new String[keys.length];
        if (options != null) {
            int i = 0;
            for (Map.Entry<String, String> entry : options.entrySet()) {
                keys[i] = entry.getKey();
                values[i] = entry.getValue();
                i++;
            }
        }
        return new FullTextIndexWriter(FullTextNative.createWriter(keys, values));
    }

    public void addDocument(long rowId, String text) {
        FullTextNative.addDocument(requireOpen(), rowId, text);
    }

    public void addDocument(long rowId, Map<String, String> fields) {
        if (fields == null) {
            throw new NullPointerException("fields");
        }
        String[] fieldNames = new String[fields.size()];
        String[] texts = new String[fields.size()];
        int i = 0;
        for (Map.Entry<String, String> entry : fields.entrySet()) {
            fieldNames[i] = entry.getKey();
            texts[i] = entry.getValue();
            i++;
        }
        FullTextNative.addDocumentFields(requireOpen(), rowId, fieldNames, texts);
    }

    /**
     * Finalizes this writer and streams the index archive to the output.
     *
     * <p>Every write attempt finalizes the native writer, even when writing or flushing fails.
     * Discard a potentially partial output and create a new writer to retry.
     */
    public void writeIndex(FullTextIndexOutput output) {
        if (output == null) {
            throw new NullPointerException("output");
        }
        FullTextNative.writeIndex(requireOpen(), output);
    }

    @Override
    public void close() {
        long ptr = nativePtr;
        nativePtr = 0;
        if (ptr != 0) {
            FullTextNative.freeWriter(ptr);
        }
    }

    private long requireOpen() {
        if (nativePtr == 0) {
            throw new IllegalStateException("FullTextIndexWriter is closed");
        }
        return nativePtr;
    }
}
