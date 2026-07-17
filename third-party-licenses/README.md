<!--
  Licensed to the Apache Software Foundation (ASF) under one
  or more contributor license agreements.  See the NOTICE file
  distributed with this work for additional information
  regarding copyright ownership.  The ASF licenses this file
  to you under the Apache License, Version 2.0 (the
  "License"); you may not use this file except in compliance
  with the License.  You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

  Unless required by applicable law or agreed to in writing,
  software distributed under the License is distributed on an
  "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
  KIND, either express or implied.  See the License for the
  specific language governing permissions and limitations
  under the License.
-->

# Third-party license sources

These exact upstream license texts fill gaps in the crates.io packages used to
build the native Java and Python artifacts:

- `jieba-rs-v0.10.1.LICENSE` is from the
  [jieba-rs v0.10.1 repository](https://github.com/messense/jieba-rs/blob/v0.10.1/LICENSE)
  and covers the `jieba-rs` and `jieba-macros` workspace crates.
- `python-jieba-v0.39.LICENSE` is from the
  [Python Jieba v0.39 repository](https://github.com/fxsjy/jieba/blob/v0.39/LICENSE)
  and covers its dictionary and HMM data embedded by `jieba-rs`.

`tools/generate_license_reports.py` includes these texts in every affected
target report and rejects dependency changes that would make the correction
set incomplete.
