#!/usr/bin/env python3
"""Tests for check-doc-links.py helpers."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-doc-links.py")
SPEC = importlib.util.spec_from_file_location("check_doc_links", SCRIPT)
assert SPEC is not None
check_doc_links = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = check_doc_links
SPEC.loader.exec_module(check_doc_links)


class NonCodeLinesTest(unittest.TestCase):
    def test_ignores_blockquoted_fenced_code(self) -> None:
        text = "\n".join(
            [
                "visible [link](target.md)",
                "> ```console",
                "> hidden [link](missing.md)",
                "> https://example.invalid",
                "> ```",
                "also visible",
            ]
        )

        self.assertEqual(
            check_doc_links.non_code_lines(text, strip_inline_code=False),
            [(1, "visible [link](target.md)"), (6, "also visible")],
        )


class MdbookSlugTest(unittest.TestCase):
    def test_dropped_punctuation_leaves_two_dashes(self) -> None:
        # mdbook emits one dash per space and does not collapse runs, so
        # punctuation it strips from between two words leaves a double dash.
        # Each expectation here was read off the generated HTML.
        cases = {
            "2. Rust → crates.io": "2-rust--cratesio",
            "5. Julia → General": "5-julia--general",
            "gRPC Server & Client Guide": "grpc-server--client-guide",
            "Request / Response Messages": "request--response-messages",
            "Integrity & maintenance": "integrity--maintenance",
        }
        for heading, expected in cases.items():
            with self.subTest(heading=heading):
                self.assertEqual(check_doc_links.mdbook_slug(heading), expected)

    def test_ordinary_headings_are_unaffected(self) -> None:
        self.assertEqual(check_doc_links.mdbook_slug("Build Prerequisites"), "build-prerequisites")
        self.assertEqual(check_doc_links.mdbook_slug("The `infrastore` CLI"), "the-infrastore-cli")


if __name__ == "__main__":
    unittest.main()
