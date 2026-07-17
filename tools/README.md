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

# Release tools

This directory contains helper scripts used by release managers and committers.

## Artifact-specific licensing

The native Java and Python packages use generated, target-specific third-party
license reports. Install the pinned generator, fetch the locked dependencies,
and regenerate the reports with:

```bash
cargo install cargo-about --version 0.9.1 --locked
cargo fetch --locked
python3 tools/generate_license_reports.py
```

CI checks that the committed reports are current:

```bash
python3 tools/generate_license_reports.py --check
```

The generator writes one report for each release target. It also includes the
nested licenses for vendored Zstandard C sources, generated Snowball algorithms,
Unicode data tables, and the Python Jieba dictionary/HMM data that are compiled
into the native libraries. It also replaces incomplete registry-package MIT
templates with the exact Jieba and Tantivy workspace licenses and rejects any
remaining copyright placeholders. The Java binary JAR contains all target
reports, while its sources and javadoc classifiers retain the plain Apache
LICENSE and NOTICE. Each Python wheel selects exactly one target report and
installs its legal files both in the import package and in the standard
`.dist-info/licenses/` directory.

Release workflows verify the assembled artifacts with:

```bash
python3 tools/verify_java_jars.py \
  --main MAIN.jar --sources SOURCES.jar --javadoc JAVADOC.jar \
  --require-all-natives
python3 tools/verify_python_wheels.py dist/*.whl
```

## Java staging deploy

`deploy_java_staging.sh` deploys the Java release candidate artifacts to Apache
Nexus staging from a committer/RM machine.

GitHub Actions does **not** sign or deploy the Java staging artifacts. The
release workflow only:

1. builds the four JNI native libraries;
2. verifies the Java release profile with GPG disabled; and
3. uploads the native libraries and verified Java jars as workflow artifacts.

The committer then runs this script locally. The script checks that the release
workflow run succeeded for the current RC tag, downloads the native libraries,
verifies their platform formats, places them into the Java resource tree, and
runs Maven locally.

### Required local setup

- `gh` GitHub CLI, authenticated with access to `apache/paimon-full-text`;
- JDK and Maven;
- local GPG setup for the release signing key;
- Maven credentials for server id `apache.releases.https`.

Maven credentials can be supplied by one of these methods:

- configure `~/.m2/settings.xml`;
- pass `--maven-settings /path/to/settings.xml`;
- set `NEXUS_STAGE_DEPLOYER_USER` and `NEXUS_STAGE_DEPLOYER_PW` so the script can
  create a temporary Maven settings file.

### Pre-flight checks

Run these checks before the first dry-run:

```bash
gh auth status
gpg --list-secret-keys --keyid-format LONG
mvn --version
```

Confirm the signing key's public key is already published in Paimon KEYS:

```text
https://downloads.apache.org/paimon/KEYS
```

Confirm Maven can use server id `apache.releases.https`. A typical
`~/.m2/settings.xml` entry is:

```xml
<settings>
  <servers>
    <server>
      <id>apache.releases.https</id>
      <username>YOUR_NEXUS_TOKEN_USER</username>
      <password>YOUR_NEXUS_TOKEN_PASSWORD</password>
    </server>
  </servers>
</settings>
```

The Nexus token is from:

```text
https://repository.apache.org/ -> Profile -> User Token
```

### Find the run id

After pushing the RC tag, open the GitHub Actions run for that RC tag. Use the
`Release` workflow run triggered by the tag, for example `v0.1.0-rc1`.

The run id is the number in the workflow run URL:

```text
https://github.com/apache/paimon-full-text/actions/runs/12345678901
```

The run id is:

```text
12345678901
```

Do not use the job id, artifact id, PR number, or commit SHA. The script checks
that this run completed successfully and that the run's commit matches the RC tag
checked out locally.

### Parameters

Required for the normal release flow:

- `--release-version 0.1.0`: Java artifact version in `java/pom.xml`. This does
  not include the RC suffix.
- `--rc 1`: RC number. Together with `--release-version`, this derives the tag
  `v0.1.0-rc1`.
- `--run-id 12345678901`: GitHub Actions run id from the RC tag's `Release`
  workflow URL. The script uses it to download the four `native-*` artifacts.

Common options:

- `--dry-run`: verify locally without signing or deploying to Nexus.
- `--maven-settings FILE`: use a specific Maven `settings.xml` containing server
  id `apache.releases.https`.
- `--staging-description TEXT`: override the Nexus staging description.
- `--no-skip-tests`: run Maven tests during dry-run or deploy.

Less common options:

- `--tag TAG`: use an explicit RC tag instead of deriving `vVERSION-rcN`.
- `--repo OWNER/REPO`: GitHub repository for `gh`; defaults to
  `apache/paimon-full-text`.
- `--no-cleanup`: keep `java/src/main/resources/native` after the script exits.
- `--skip-native-file-check`: skip native binary format checks.

The last option is an escape hatch. Avoid it for normal releases.

### Dry-run before publishing

Always run a dry-run first with the real RC workflow artifacts:

```bash
./tools/deploy_java_staging.sh \
  --release-version 0.1.0 \
  --rc 1 \
  --run-id 12345678901 \
  --dry-run
```

Dry-run mode validates the GitHub Actions run id, downloads the native
libraries, and runs:

```bash
mvn clean verify -Prelease -Dgpg.skip=true -DskipTests
```

It does not sign and does not deploy to Nexus. It verifies:

- `java/pom.xml` version matches `--release-version`;
- current checkout matches the RC tag, such as `v0.1.0-rc1`;
- Java package inputs have no local changes;
- the GitHub Actions run is a successful tag-push `Release` workflow run and its
  commit matches the RC tag;
- all four native libraries are present;
- native library file formats match their target platforms;
- the Java jar, sources jar, and javadoc jar are produced;
- the Java jar contains all four native library entries and all four matching
  third-party license reports; and
- the sources and javadoc jars contain only their own Apache legal metadata,
  without binary-only license declarations.

### Deploy to Nexus staging

After the dry-run succeeds, run the same command without `--dry-run`:

```bash
./tools/deploy_java_staging.sh \
  --release-version 0.1.0 \
  --rc 1 \
  --run-id 12345678901
```

The script repeats the local preflight before creating any remote staging
artifacts:

```bash
mvn clean verify -Prelease -Dgpg.skip=true -DskipTests
```

After that passes, it runs the local Nexus staging deploy:

```bash
mvn deploy -Prelease -DskipTests \
  -DstagingDescription="Apache Paimon Full Text, version 0.1.0, release candidate 1"
```

The Maven output contains the Nexus staging repository id, for example:

```text
orgapachepaimon-XXXX
```

Use that id in the release vote email.
