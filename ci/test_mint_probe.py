#!/usr/bin/env python3
"""P2 tests -- the live-mint lane must never turn "could not read" into a verdict.

Run: python3 ci/test_mint_probe.py

These are deliberately offline: every case drives the classifier and the
error-paths, because the failure mode worth testing is not "does HTTP work" but
"does an unreadable mint silently become a pass". A test that needed the network
would be untestable in CI and would not protect the property that matters.
"""

import os
import sys
import unittest

sys.path.insert(0, __file__.rsplit("/", 1)[0])

import mint_probe as m  # noqa: E402


class TestClassify(unittest.TestCase):
    def test_all_unspent_is_ok(self):
        self.assertEqual(m.classify({"Y1": "UNSPENT", "Y2": "UNSPENT"}), "ok")

    def test_any_spent_is_spent(self):
        self.assertEqual(m.classify({"Y1": "UNSPENT", "Y2": "SPENT"}), "spent")
        self.assertEqual(m.classify({"Y1": "SPENT"}), "spent")

    def test_pending_is_not_a_pass(self):
        # PENDING is transient. Calling it "ok" would certify a proof that may
        # still be spent by someone else.
        with self.assertRaises(m.MintError):
            m.classify({"Y1": "PENDING"})

    def test_unknown_vocabulary_is_an_error(self):
        # A mint answering something we do not understand must not be coerced
        # into a verdict.
        with self.assertRaises(m.MintError):
            m.classify({"Y1": "WEIRD_FUTURE_STATE"})

    def test_empty_checkstate_is_an_error(self):
        with self.assertRaises(m.MintError):
            m.classify({})


class TestErrorNeverBecomesVerdict(unittest.TestCase):
    def test_unreachable_mint_raises(self):
        # Port 1 on loopback: connection refused, not a hang.
        with self.assertRaises(m.MintError):
            m.mint_info("http://127.0.0.1:1")

    def test_checkstate_unreachable_raises(self):
        with self.assertRaises(m.MintError):
            m.check_state("http://127.0.0.1:1", ["Y1"])

    def test_malformed_response_is_not_a_verdict(self):
        # Simulate the shape error by monkeypatching the transport.
        orig = m._post_json

        def fake(url, payload, timeout=20.0):
            return {"notstates": []}

        m._post_json = fake
        try:
            with self.assertRaises(m.MintError):
                m.check_state("http://example.invalid", ["Y1"])
        finally:
            m._post_json = orig


class TestTokenDecode(unittest.TestCase):
    def test_v4_without_cdk_cli_is_an_error_not_silence(self):
        # ADR-0003: V4 decoding is delegated to cdk-cli. If it is absent the answer
        # must be an error naming the missing tool -- never an empty proof list,
        # which would read as "nothing to check".
        orig = os.environ.get("CDK_CLI")
        os.environ["CDK_CLI"] = "/nonexistent/cdk-cli-for-test"
        try:
            with self.assertRaises(m.MintError) as ctx:
                m.decode_token("cashuB" + "A" * 40)
            self.assertIn("cdk-cli", str(ctx.exception).lower())
        finally:
            if orig is None:
                os.environ.pop("CDK_CLI", None)
            else:
                os.environ["CDK_CLI"] = orig

    def test_v4_garbage_is_an_error(self):
        # cdk-cli exists but rejects it -> error, not an empty result.
        with self.assertRaises(m.MintError):
            m.decode_token("cashuB" + "A" * 40)

    def test_garbage_is_refused(self):
        with self.assertRaises(m.MintError):
            m.decode_token("notatoken")

    def test_v3_legacy_decodes(self):
        import base64
        import json

        payload = {"m": "https://mint.example", "proofs": [{"secret": "s"}]}
        body = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
        mint, proofs = m.decode_token("cashuA" + body)
        self.assertEqual(mint, "https://mint.example")
        self.assertEqual(len(proofs), 1)


class TestVocabulary(unittest.TestCase):
    def test_known_verdicts_match_corpus(self):
        # If the corpus vocabulary grows, this test is the reminder to update
        # KNOWN_VERDICTS rather than silently producing an out-of-vocabulary value.
        for v in ("ok", "spent", "unknown"):
            self.assertTrue(m.verdict_is_known(v))
        self.assertFalse(m.verdict_is_known("definitely-not-a-verdict"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
