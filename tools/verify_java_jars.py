#!/usr/bin/env python3

# Licensed to the Apache Software Foundation (ASF) under one or more
# contributor license agreements.  See the NOTICE file distributed with
# this work for additional information regarding copyright ownership.
# The ASF licenses this file to You under the Apache License, Version 2.0
# (the "License"); you may not use this file except in compliance with
# the License.  You may obtain a copy of the License at
#
#    http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Verify main and classifier JAR licensing matches their bundled content."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path
from zipfile import ZipFile


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
NATIVE_ENTRIES = {
    "native/linux/x86_64/libpaimon_ftindex_jni.so",
    "native/linux/aarch64/libpaimon_ftindex_jni.so",
    "native/macos/aarch64/libpaimon_ftindex_jni.dylib",
    "native/windows/x86_64/paimon_ftindex_jni.dll",
}
NESTED_LICENSE_MARKERS = (
    "For Zstandard software",
    "Dr Martin Porter",
    "UNICODE, INC. LICENSE AGREEMENT",
    'id="mit-jieba-rs-workspace"',
    "Copyright (c) 2018 - 2019 messense",
    "Copyright (c) 2019 Paul Meng",
    'id="bundled-python-jieba-data"',
    "Copyright (c) 2013 Sun Junyi",
)
LICENSE_PLACEHOLDERS = (
    "&lt;year&gt;",
    "&lt;copyright holders&gt;",
    "<year>",
    "<copyright holders>",
)


def repository_root() -> Path:
    return Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )


def verify_notice(archive: ZipFile, root: Path) -> None:
    expected = (root / "NOTICE").read_bytes()
    actual = archive.read("META-INF/NOTICE")
    if actual != expected:
        raise ValueError("META-INF/NOTICE does not match the canonical NOTICE")


def verify_main_jar(path: Path, root: Path, require_all_natives: bool) -> None:
    binary_resources = root / "java/src/main/binary-resources/META-INF"
    with ZipFile(path) as archive:
        names = set(archive.namelist())
        required = {"META-INF/LICENSE", "META-INF/NOTICE"}
        required.update(
            f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
            for target in TARGETS
        )
        missing = sorted(required - names)
        if missing:
            raise ValueError(f"missing legal files: {missing}")
        verify_notice(archive, root)
        if "META-INF/DEPENDENCIES.rust.tsv" in names:
            raise ValueError(
                "main JAR contains the cross-target repository dependency inventory"
            )

        expected_license = (binary_resources / "LICENSE").read_bytes()
        if archive.read("META-INF/LICENSE") != expected_license:
            raise ValueError("main JAR LICENSE is not the binary-specific LICENSE")
        license_text = expected_license.decode("utf-8")

        for target in TARGETS:
            report_path = f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
            if report_path not in license_text:
                raise ValueError(f"LICENSE does not point to {report_path}")
            expected_report = (
                binary_resources
                / "licenses"
                / target
                / "THIRD-PARTY-LICENSES.html"
            ).read_bytes()
            actual_report = archive.read(report_path)
            if actual_report != expected_report:
                raise ValueError(f"{report_path} differs from its generated source")

            report_text = actual_report.decode("utf-8")
            if target not in report_text:
                raise ValueError(f"{report_path} does not identify its target")
            for marker in NESTED_LICENSE_MARKERS:
                if marker not in report_text:
                    raise ValueError(f"{report_path} is missing {marker!r}")
            for placeholder in LICENSE_PLACEHOLDERS:
                if placeholder in report_text:
                    raise ValueError(
                        f"{report_path} contains license placeholder {placeholder!r}"
                    )
            if target.endswith("linux-gnu"):
                if "linux-raw-sys" not in report_text or "windows_x86_64_" in report_text:
                    raise ValueError(f"{report_path} has an incorrect target dependency set")
            elif target == "aarch64-apple-darwin":
                if "linux-raw-sys" in report_text or "windows_x86_64_" in report_text:
                    raise ValueError(f"{report_path} has an incorrect target dependency set")
            else:
                if "windows_x86_64_msvc" not in report_text:
                    raise ValueError(f"{report_path} is missing the MSVC target package")
                if "windows_x86_64_gnu" in report_text or "linux-raw-sys" in report_text:
                    raise ValueError(f"{report_path} has an incorrect target dependency set")

        packaged_natives = {name for name in names if name.startswith("native/") and not name.endswith("/")}
        unexpected_natives = packaged_natives - NATIVE_ENTRIES
        if unexpected_natives:
            raise ValueError(f"unexpected native entries: {sorted(unexpected_natives)}")
        if require_all_natives and packaged_natives != NATIVE_ENTRIES:
            raise ValueError(
                "release JAR native entries differ from the four declared targets: "
                + repr(sorted(packaged_natives))
            )

    print(f"verified main JAR: {path}")


def verify_classifier(path: Path, root: Path) -> None:
    with ZipFile(path) as archive:
        names = set(archive.namelist())
        for required in ("META-INF/LICENSE", "META-INF/NOTICE"):
            if required not in names:
                raise ValueError(f"missing {required}")
        verify_notice(archive, root)

        license_text = archive.read("META-INF/LICENSE").decode("utf-8")
        if "Apache License" not in license_text:
            raise ValueError("LICENSE does not contain the Apache License")
        if "BUNDLED THIRD-PARTY COMPONENTS" in license_text:
            raise ValueError("classifier LICENSE incorrectly describes native components")

        forbidden = sorted(
            name
            for name in names
            if name.startswith("native/")
            or name == "META-INF/DEPENDENCIES.rust.tsv"
            or name.endswith("/THIRD-PARTY-LICENSES.html")
        )
        if forbidden:
            raise ValueError(f"classifier contains binary-only files: {forbidden}")

    print(f"verified classifier JAR: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--main", required=True, type=Path)
    parser.add_argument("--sources", required=True, type=Path)
    parser.add_argument("--javadoc", required=True, type=Path)
    parser.add_argument("--require-all-natives", action="store_true")
    args = parser.parse_args()
    root = repository_root()

    try:
        verify_main_jar(args.main, root, args.require_all_natives)
        verify_classifier(args.sources, root)
        verify_classifier(args.javadoc, root)
    except (KeyError, OSError, ValueError) as error:
        print(f"Java artifact verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
