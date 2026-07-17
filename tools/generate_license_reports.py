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

"""Generate artifact-exact third-party license reports for native binaries."""

from __future__ import annotations

import argparse
import difflib
import html
import json
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


CARGO_ABOUT_VERSION = "0.9.1"
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


@dataclass(frozen=True)
class Report:
    manifest: str
    target: str
    output: str


@dataclass(frozen=True)
class BundledComponent:
    crate: str
    license_path: str
    component: str
    component_url: str
    license_name: str
    anchor: str
    license_from_repository: bool = False


@dataclass(frozen=True)
class LicenseCorrection:
    crates: tuple[str, ...]
    license_crate: str
    license_path: str
    license_name: str
    anchor: str


BUNDLED_COMPONENTS = (
    BundledComponent(
        crate="zstd-sys",
        license_path="zstd/LICENSE",
        component="vendored Zstandard C sources",
        component_url="https://github.com/facebook/zstd",
        license_name="BSD 3-Clause License",
        anchor="bundled-zstandard-bsd-3-clause",
    ),
    BundledComponent(
        crate="rust-stemmers",
        license_path="algorithms/LICENSE",
        component="generated Snowball stemming algorithms",
        component_url="https://snowballstem.org/",
        license_name="BSD 3-Clause License",
        anchor="bundled-snowball-bsd-3-clause",
    ),
    BundledComponent(
        crate="regex-syntax",
        license_path="src/unicode_tables/LICENSE-UNICODE",
        component="generated Unicode character database tables",
        component_url="https://www.unicode.org/",
        license_name="Unicode Data Files and Software License",
        anchor="bundled-regex-syntax-unicode",
    ),
    BundledComponent(
        crate="jieba-rs",
        license_path="third-party-licenses/python-jieba-v0.39.LICENSE",
        component="embedded Python Jieba dictionary and HMM data",
        component_url="https://github.com/fxsjy/jieba/tree/v0.39",
        license_name="MIT License",
        anchor="bundled-python-jieba-data",
        license_from_repository=True,
    ),
)


LICENSE_CORRECTIONS = (
    LicenseCorrection(
        crates=("jieba-macros", "jieba-rs"),
        license_crate="",
        license_path="third-party-licenses/jieba-rs-v0.10.1.LICENSE",
        license_name="MIT License (jieba-rs workspace)",
        anchor="mit-jieba-rs-workspace",
    ),
    LicenseCorrection(
        crates=(
            "ownedbytes",
            "tantivy-bitpacker",
            "tantivy-columnar",
            "tantivy-common",
            "tantivy-query-grammar",
            "tantivy-sstable",
            "tantivy-stacker",
            "tantivy-tokenizer-api",
        ),
        license_crate="tantivy",
        license_path="LICENSE",
        license_name="MIT License (Tantivy workspace)",
        anchor="mit-tantivy-workspace",
    ),
)

PLACEHOLDER_MIT_MARKER = (
    "Copyright (c) &lt;year&gt; &lt;copyright holders&gt;"
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


def report_specs() -> list[Report]:
    reports = []
    for target in TARGETS:
        reports.append(
            Report(
                manifest="jni/Cargo.toml",
                target=target,
                output=(
                    "java/src/main/binary-resources/META-INF/licenses/"
                    f"{target}/THIRD-PARTY-LICENSES.html"
                ),
            )
        )
        reports.append(
            Report(
                manifest="ffi/Cargo.toml",
                target=target,
                output=f"python/licenses/{target}/THIRD-PARTY-LICENSES.html",
            )
        )
    return reports


def verify_cargo_about(root: Path) -> None:
    output = subprocess.check_output(
        ["cargo", "about", "--version"], cwd=root, text=True
    ).strip()
    actual = output.rsplit(" ", 1)[-1]
    if actual != CARGO_ABOUT_VERSION:
        raise RuntimeError(
            f"cargo-about {CARGO_ABOUT_VERSION} is required, found {output!r}"
        )


def cargo_metadata(root: Path, report: Report) -> dict:
    output = subprocess.check_output(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
            report.manifest,
            "--filter-platform",
            report.target,
        ],
        cwd=root,
        text=True,
    )
    return json.loads(output)


def generate_base_report(root: Path, report: Report, output: Path) -> str:
    subprocess.run(
        [
            "cargo",
            "about",
            "generate",
            "--frozen",
            "--fail",
            "--config",
            str(root / "about.toml"),
            "--manifest-path",
            report.manifest,
            "--target",
            report.target,
            "--output-file",
            str(output),
            str(root / "about.hbs"),
        ],
        cwd=root,
        check=True,
    )
    return output.read_text(encoding="utf-8")


def package_by_name(metadata: dict, crate_name: str) -> dict:
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    matches = [
        package
        for package in metadata["packages"]
        if package["id"] in resolved and package["name"] == crate_name
    ]
    if len(matches) != 1:
        versions = [package["version"] for package in matches]
        raise RuntimeError(
            f"expected exactly one resolved {crate_name} package, found {versions}"
        )
    return matches[0]


def correction_license_file(
    root: Path, metadata: dict, correction: LicenseCorrection
) -> Path:
    if not correction.license_crate:
        return root / correction.license_path
    license_package = package_by_name(metadata, correction.license_crate)
    return Path(license_package["manifest_path"]).parent / correction.license_path


def license_correction_html(
    root: Path, metadata: dict, correction: LicenseCorrection
) -> str:
    used_by = []
    for crate_name in correction.crates:
        package = package_by_name(metadata, crate_name)
        repository = package.get("repository") or (
            f"https://crates.io/crates/{crate_name}"
        )
        used_by.append(
            f'                    <li><a href="{html.escape(repository, quote=True)}">'
            f"{html.escape(crate_name)} {html.escape(package['version'])}</a></li>"
        )

    license_file = correction_license_file(root, metadata, correction)
    if not license_file.is_file():
        raise RuntimeError(f"corrected license file is missing: {license_file}")
    license_text = license_file.read_text(encoding="utf-8")

    return "\n".join(
        [
            '            <li class="license corrected-workspace-license">',
            f'                <h3 id="{html.escape(correction.anchor)}">'
            f"{html.escape(correction.license_name)}</h3>",
            "                <h4>Used by:</h4>",
            '                <ul class="license-used-by">',
            *used_by,
            "                </ul>",
            f'                <pre class="license-text">{html.escape(license_text)}</pre>',
            "            </li>",
        ]
    )


def replace_placeholder_mit_license(
    root: Path, base_report: str, metadata: dict
) -> str:
    if base_report.count(PLACEHOLDER_MIT_MARKER) != 1:
        raise RuntimeError(
            "expected exactly one generic MIT copyright placeholder in "
            "cargo-about output"
        )

    marker_index = base_report.index(PLACEHOLDER_MIT_MARKER)
    entry_start = base_report.rfind(
        '            <li class="license">', 0, marker_index
    )
    entry_end = base_report.find("            </li>", marker_index)
    if entry_start == -1 or entry_end == -1:
        raise RuntimeError("could not locate the generic MIT license entry")
    entry_end += len("            </li>")
    placeholder_entry = base_report[entry_start:entry_end]

    actual_crates = set(
        re.findall(
            r'<li><a href="[^"]+">([A-Za-z0-9_-]+) [^<]+</a></li>',
            placeholder_entry,
        )
    )
    expected_crates = {
        crate_name
        for correction in LICENSE_CORRECTIONS
        for crate_name in correction.crates
    }
    if actual_crates != expected_crates:
        raise RuntimeError(
            "generic MIT placeholder dependency set changed: expected "
            f"{sorted(expected_crates)}, found {sorted(actual_crates)}"
        )

    replacement = "\n".join(
        license_correction_html(root, metadata, correction)
        for correction in LICENSE_CORRECTIONS
    )
    result = base_report[:entry_start] + replacement + base_report[entry_end:]
    for placeholder in LICENSE_PLACEHOLDERS:
        if placeholder in result:
            raise RuntimeError(
                f"generated report still contains license placeholder {placeholder!r}"
            )
    return result


def bundled_component_html(root: Path, base_report: str, metadata: dict) -> str:
    items = []
    for component in BUNDLED_COMPONENTS:
        package = package_by_name(metadata, component.crate)
        marker = f">{component.crate} {package['version']}</a>"
        if marker not in base_report:
            raise RuntimeError(
                f"cargo-about report omitted resolved crate {component.crate} "
                f"{package['version']}"
            )

        crate_root = Path(package["manifest_path"]).parent
        license_file = (
            root / component.license_path
            if component.license_from_repository
            else crate_root / component.license_path
        )
        if not license_file.is_file():
            raise RuntimeError(f"bundled license file is missing: {license_file}")
        license_text = license_file.read_text(encoding="utf-8")
        repository = package.get("repository") or (
            f"https://crates.io/crates/{component.crate}"
        )

        items.append(
            "\n".join(
                [
                    '            <li class="license bundled-subcomponent">',
                    f'                <h3 id="{html.escape(component.anchor)}">'
                    f"{html.escape(component.license_name)}</h3>",
                    "                <h4>Bundled component:</h4>",
                    '                <ul class="license-used-by">',
                    "                    <li>",
                    f'                        <a href="{html.escape(component.component_url, quote=True)}">'
                    f"{html.escape(component.component)}</a>, bundled by",
                    f'                        <a href="{html.escape(repository, quote=True)}">'
                    f"{html.escape(component.crate)} {html.escape(package['version'])}</a>",
                    "                    </li>",
                    "                </ul>",
                    f'                <pre class="license-text">{html.escape(license_text)}</pre>',
                    "            </li>",
                ]
            )
        )

    return "\n".join(
        [
            "",
            "        <h2>Licenses for source components bundled inside crates:</h2>",
            "        <p>",
            "            The following components are compiled into the native library but",
            "            have licenses outside their published package-level metadata, so",
            "            they require explicit entries in addition to the crate licenses above.",
            "        </p>",
            '        <ul class="licenses-list bundled-subcomponents">',
            *items,
            "        </ul>",
        ]
    )


def complete_report(
    root: Path, base_report: str, report: Report, metadata: dict
) -> str:
    base_report = replace_placeholder_mit_license(root, base_report, metadata)
    description = (
        "\n        <p><strong>Rust target:</strong> "
        f"<code>{html.escape(report.target)}</code></p>"
        "\n        <p><strong>Root crate:</strong> "
        f"<code>{html.escape(Path(report.manifest).parent.name)}</code></p>"
    )
    first_paragraph_end = base_report.find("</p>")
    if first_paragraph_end == -1:
        raise RuntimeError("about.hbs output has no introductory paragraph")
    first_paragraph_end += len("</p>")
    result = (
        base_report[:first_paragraph_end]
        + description
        + base_report[first_paragraph_end:]
    )

    closing_main = result.rfind("    </main>")
    if closing_main == -1:
        raise RuntimeError("about.hbs output has no closing main element")
    result = (
        result[:closing_main]
        + bundled_component_html(root, base_report, metadata)
        + "\n"
        + result[closing_main:]
    )
    # Some upstream license files contain insignificant trailing spaces. Keep
    # generated reports friendly to git's whitespace checks without changing
    # any license wording.
    return "\n".join(line.rstrip() for line in result.rstrip().splitlines()) + "\n"


def binary_license(apache_license: str, heading: str, details: list[str]) -> str:
    appendix = [
        "",
        "=" * 79,
        "BUNDLED THIRD-PARTY COMPONENTS",
        "=" * 79,
        "",
        heading,
        "The component inventory, copyright notices, and complete license texts",
        "are provided in:",
        "",
    ]
    appendix.extend(f"    {detail}" for detail in details)
    return apache_license.rstrip() + "\n" + "\n".join(appendix) + "\n"


def generated_files(root: Path) -> dict[Path, str]:
    verify_cargo_about(root)
    result = {}
    with tempfile.TemporaryDirectory(prefix="paimon-license-reports-") as temp_dir:
        temp_root = Path(temp_dir)
        for index, report in enumerate(report_specs()):
            base = generate_base_report(
                root, report, temp_root / f"report-{index}.html"
            )
            metadata = cargo_metadata(root, report)
            result[root / report.output] = complete_report(
                root, base, report, metadata
            )

    apache_license = (root / "LICENSE").read_text(encoding="utf-8")
    notice = (root / "NOTICE").read_text(encoding="utf-8").rstrip() + "\n"

    java_report_paths = [
        f"META-INF/licenses/{target}/THIRD-PARTY-LICENSES.html"
        for target in TARGETS
    ]
    java_license = binary_license(
        apache_license,
        "This binary JAR bundles Rust native libraries for four release targets.",
        java_report_paths,
    )
    result[
        root / "java/src/main/binary-resources/META-INF/LICENSE"
    ] = java_license

    for target in TARGETS:
        license_dir = root / "python/licenses" / target
        result[license_dir / "LICENSE"] = binary_license(
            apache_license,
            f"This binary wheel bundles the Rust native library for {target}.",
            ["THIRD-PARTY-LICENSES.html"],
        )
        result[license_dir / "NOTICE"] = notice

    return result


def check_files(files: dict[Path, str], root: Path) -> int:
    failed = False
    for path, expected in files.items():
        if not path.is_file():
            print(f"missing generated license file: {path.relative_to(root)}")
            failed = True
            continue
        actual = path.read_text(encoding="utf-8")
        if actual == expected:
            continue
        failed = True
        print(f"stale generated license file: {path.relative_to(root)}")
        diff = difflib.unified_diff(
            actual.splitlines(),
            expected.splitlines(),
            fromfile=str(path.relative_to(root)),
            tofile=f"generated/{path.relative_to(root)}",
            lineterm="",
        )
        for line in list(diff)[:200]:
            print(line)
    return 1 if failed else 0


def write_files(files: dict[Path, str], root: Path) -> None:
    for path, content in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        print(f"generated {path.relative_to(root)}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if checked-in reports differ from reproducible output",
    )
    args = parser.parse_args()

    root = repository_root()
    try:
        files = generated_files(root)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"failed to generate license reports: {error}", file=sys.stderr)
        return 1

    if args.check:
        return check_files(files, root)
    write_files(files, root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
