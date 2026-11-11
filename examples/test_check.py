"""Tests of the example check command: python3 -B examples/test_check.py (part of ./build.sh --tests)."""

import getpass
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from datetime import date
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import check  # noqa: E402

TODAY = date(2026, 11, 11)


def view(body, attachments=(), thread=None):
    return {
        "account": "work",
        "from": {"address": "me@example.org", "name": "Me"},
        "to": ["Ann <ann@example.org>"],
        "cc": [],
        "bcc": [],
        "subject": "Re: Offer",
        "body": body,
        "body_sha256": "0" * 64,
        "reply": None,
        "thread": thread,
        "attachments": [{"name": n, "size": 10} for n in attachments],
    }


def fake_claude(folder, stdout, code=0):
    """An executable that answers like `claude -p --output-format json` and keeps its arguments and input."""
    path = Path(folder) / "claude"
    path.write_text(
        "#!" + sys.executable + "\n"
        "import json, pathlib, sys\n"
        "here = pathlib.Path(__file__).parent\n"
        "(here / 'argv.json').write_text(json.dumps(sys.argv[1:]))\n"
        "(here / 'stdin.txt').write_text(sys.stdin.read())\n"
        "sys.stdout.write(" + repr(stdout) + ")\n"
        "sys.exit(" + str(code) + ")\n"
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


def envelope(findings):
    return json.dumps(
        {
            "type": "result",
            "is_error": False,
            "structured_output": {"findings": findings},
        }
    )


class Dates(unittest.TestCase):
    def finding(self, text):
        found = check.date_findings(text, TODAY)
        self.assertEqual(len(found), 1, found)
        return found[0]

    def test_wrong_weekday_blocks(self):
        for text, quote in [
            ("See you on Monday, 17 November 2026.", "Monday, 17 November 2026"),
            ("Termin am Montag, 17.11.2026 um 10 Uhr.", "Montag, 17.11.2026"),
            ("Call on Mon 17.11.", "Mon 17.11."),
            ("Bis Mi., den 17. November.", "Mi., den 17. November"),
            ("Thursday, November 17th works.", "Thursday, November 17th"),
            ("Delivery 2026-11-17 (Friday).", "2026-11-17 (Friday)"),
            ("Am 17.11. (Donnerstag) passt es.", "17.11. (Donnerstag)"),
        ]:
            with self.subTest(text=text):
                f = self.finding(text)
                self.assertEqual(f["level"], "block")
                self.assertEqual(f["quote"], quote)
                self.assertIn("Tuesday", f["detail"])

    def test_right_weekday_passes(self):
        for text in [
            "See you on Tuesday, 17 November 2026.",
            "Termin am Dienstag, 17.11.26.",
            "Wed 18 Nov",
            "Mittwoch, den 18. November",
            "Tuesday, November 17, 2026",
            "Fr., 13.11.",
        ]:
            with self.subTest(text=text):
                self.assertEqual(check.date_findings(text, TODAY), [])

    def test_no_such_date_blocks(self):
        f = self.finding("Am Montag, 31.11. bin ich da.")
        self.assertEqual(f["level"], "block")
        self.assertEqual(f["quote"], "Montag, 31.11.")

    def test_a_date_without_year_is_the_next_one_from_60_days_ago_on(self):
        # On 5 January 2027, 29 December is the one of 2026 (a Tuesday); on 29 December, 5 January is in 2027.
        self.assertEqual(
            check.date_findings("Tuesday, 29 December", date(2027, 1, 5)), []
        )
        self.assertEqual(
            check.date_findings("Tuesday, 5 January", date(2026, 12, 29)), []
        )

    def test_quoted_lines_are_not_checked(self):
        self.assertEqual(
            check.date_findings("Thanks.\n> Monday, 17 November 2026\n", TODAY), []
        )

    def test_dates_without_weekday_and_plain_words_are_left_alone(self):
        self.assertEqual(
            check.date_findings(
                "Version 17.11.2 is out. So 3 items, do 5 more.", TODAY
            ),
            [],
        )


class Attachments(unittest.TestCase):
    def test_mentioned_but_missing(self):
        for body, word in [
            ("Please find attached the offer.", "attached"),
            ("Anbei das Angebot.", "Anbei"),
            ("Das Angebot sende ich im Anhang.", "Anhang"),
        ]:
            with self.subTest(body=body):
                (f,) = check.attachment_findings(body, [])
                self.assertEqual(
                    (f["level"], f["confirm"], f["quote"]), ("warn", True, word)
                )

    def test_present_or_only_quoted(self):
        self.assertEqual(
            check.attachment_findings(
                "Anbei das Angebot.", [{"name": "a.pdf", "size": 1}]
            ),
            [],
        )
        self.assertEqual(
            check.attachment_findings("Thanks!\n> see the attached file\n", []), []
        )
        self.assertEqual(check.attachment_findings("Thanks for the offer.", []), [])


class SecondReader(unittest.TestCase):
    def test_findings_are_kept_and_cleaned(self):
        body = "Hello Ann,\n\nthe price is 500 EUR.\n"
        answer = envelope(
            [
                {
                    "level": "info",
                    "title": "Price",
                    "detail": "Confirm the price.",
                    "quote": "500 EUR",
                    "confirm": True,
                },
                {
                    "level": "block",
                    "title": "Made up",
                    "detail": "x",
                    "quote": "not in the draft",
                    "confirm": False,
                },
            ]
        )
        with tempfile.TemporaryDirectory() as d:
            exe = fake_claude(d, answer)
            found = check.second_reader(
                view(body, thread="Ann: what is the price?"), exe, "some-model", 30
            )
            argv = json.loads((Path(d) / "argv.json").read_text())
            sent = (Path(d) / "stdin.txt").read_text()
        self.assertEqual(found[0]["quote"], "500 EUR")
        self.assertTrue(found[0]["confirm"])
        self.assertEqual(found[1]["level"], "warn", "the second reader never blocks")
        self.assertEqual(found[1]["quote"], "", "a quote not in the draft is dropped")
        for flag in [
            "-p",
            "--no-session-persistence",
            "--strict-mcp-config",
            "--json-schema",
        ]:
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("--tools") + 1], "")
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "")
        self.assertEqual(argv[argv.index("--model") + 1], "some-model")
        self.assertIn("what is the price?", sent)
        self.assertIn("500 EUR", sent)

    def test_failure_is_a_warning_never_silence(self):
        with tempfile.TemporaryDirectory() as d:
            for out, code in [
                ("", 1),
                ("not json", 0),
                (json.dumps({"is_error": True, "result": "x"}), 0),
            ]:
                with self.subTest(out=out, code=code):
                    (f,) = check.second_reader(
                        view("Hi"), fake_claude(d, out, code), None, 30
                    )
                    self.assertEqual(f["level"], "warn")

    def test_the_reason_of_a_failed_run_is_shown(self):
        # claude -p puts "Not logged in" into the JSON on stdout and ends with exit 1.
        out = json.dumps({"is_error": True, "result": "Not logged in"})
        with tempfile.TemporaryDirectory() as d:
            (f,) = check.second_reader(view("Hi"), fake_claude(d, out, 1), None, 30)
        self.assertIn("Not logged in", f["detail"])

    def test_the_user_name_is_passed_on(self):
        # Draft Desk starts the check with PATH, HOME and LANG only; without USER claude misses its sign-in.
        exe_env = {k: v for k, v in os.environ.items() if k not in ("USER", "LOGNAME")}
        with tempfile.TemporaryDirectory() as d:
            exe = Path(d) / "claude"
            exe.write_text(
                "#!/bin/sh\n"
                'printf \'{"is_error":false,"structured_output":{"findings":'
                '[{"level":"info","title":"%s","detail":"","quote":"","confirm":false}]}}\' "$USER"\n'
            )
            exe.chmod(0o700)
            with mock.patch.dict(os.environ, exe_env, clear=True):
                (f,) = check.second_reader(view("Hi"), str(exe), None, 30)
        self.assertEqual(f["title"], getpass.getuser())

    def test_time_limit(self):
        with tempfile.TemporaryDirectory() as d:
            exe = Path(d) / "claude"
            exe.write_text("#!/bin/sh\nsleep 5\n")
            exe.chmod(0o700)
            (f,) = check.second_reader(view("Hi"), str(exe), None, 1)
        self.assertEqual(f["level"], "warn")
        self.assertIn("1 s", f["detail"])


class Command(unittest.TestCase):
    def run_check(self, item, *args):
        script = str(HERE / "check.py")
        env = {"PATH": "/usr/bin:/bin", "HOME": os.environ.get("HOME", "/tmp")}
        return subprocess.run(
            [script, *args],
            input=json.dumps(item),
            capture_output=True,
            text=True,
            env=env,
            timeout=60,
        )

    def test_answers_in_the_check_format(self):
        out = self.run_check(
            view("Anbei das Angebot. Termin: Montag, 17.11.2026."), "--no-claude"
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        result = json.loads(out.stdout)
        self.assertEqual(result["verdict"], "block")
        self.assertEqual([f["level"] for f in result["findings"]], ["block", "warn"])
        for f in result["findings"]:
            self.assertEqual(set(f), {"level", "title", "detail", "quote", "confirm"})

    def test_clean_draft_passes(self):
        out = self.run_check(view("Hello Ann,\n\nthanks.\n"), "--no-claude")
        self.assertEqual(json.loads(out.stdout), {"verdict": "pass", "findings": []})

    def test_second_reader_runs_when_claude_is_given(self):
        with tempfile.TemporaryDirectory() as d:
            exe = fake_claude(
                d,
                envelope(
                    [
                        {
                            "level": "warn",
                            "title": "t",
                            "detail": "d",
                            "quote": "",
                            "confirm": False,
                        }
                    ]
                ),
            )
            out = self.run_check(view("Hello Ann,\n\nthanks.\n"), "--claude", exe)
        self.assertEqual(json.loads(out.stdout)["verdict"], "warn")

    def test_no_claude_found_is_noted(self):
        out = self.run_check(
            view("Hello Ann,\n\nthanks.\n"), "--claude", "/nonexistent/claude"
        )
        result = json.loads(out.stdout)
        self.assertEqual(result["verdict"], "pass")
        self.assertEqual([f["level"] for f in result["findings"]], ["info"])

    def test_broken_input_is_an_error(self):
        out = subprocess.run(
            [str(HERE / "check.py"), "--no-claude"],
            input="{",
            capture_output=True,
            text=True,
        )
        self.assertEqual(out.returncode, 1)
        self.assertEqual(out.stdout, "")


class ReviewFindings(unittest.TestCase):
    """Cases the review of this example found."""

    def test_english_do_and_so_are_no_weekdays(self):
        for text in [
            "We can do 17 November 2026.",
            "So 17 November 2026 works for me.",
        ]:
            with self.subTest(text=text):
                self.assertEqual(check.date_findings(text, TODAY), [])
        (f,) = check.date_findings("Do., 17.11.2026", TODAY)
        self.assertEqual(f["quote"], "Do., 17.11.2026")
        self.assertEqual(
            check.date_findings("Do., 19.11.2026 und So., 22.11.", TODAY), []
        )

    def test_a_time_is_no_date(self):
        self.assertEqual(check.date_findings("Montag, 10.30. Uhr", TODAY), [])

    def test_more_words_for_an_attachment(self):
        for body, word in [
            ("Siehe Anhang.", "Anhang"),
            ("The enclosure shows it.", "enclosure"),
        ]:
            with self.subTest(body=body):
                (f,) = check.attachment_findings(body, [])
                self.assertEqual(f["quote"], word)

    def test_an_invalid_finding_of_the_model_is_never_a_silent_pass(self):
        answer = json.dumps(
            {"is_error": False, "structured_output": {"findings": [None]}}
        )
        with tempfile.TemporaryDirectory() as d:
            (f,) = check.second_reader(view("Hi"), fake_claude(d, answer), None, 30)
        self.assertEqual(
            (f["level"], f["title"]), ("warn", "Second reader did not run")
        )

    def test_a_relative_claude_path_still_works_from_the_temporary_folder(self):
        with tempfile.TemporaryDirectory() as d:
            fake_claude(d, envelope([]))
            old = os.getcwd()
            os.chdir(d)
            try:
                found = check.find_claude("./claude")
            finally:
                os.chdir(old)
            self.assertEqual(found, str(Path(d).resolve() / "claude"))
            self.assertEqual(check.second_reader(view("Hi"), found, None, 30), [])

    def test_german_dem_before_the_date(self):
        (f,) = check.date_findings("Termin am Montag, dem 17.11.2026.", TODAY)
        self.assertEqual(f["quote"], "Montag, dem 17.11.2026")

    def test_a_sentence_end_or_a_line_break_parts_weekday_and_date(self):
        for text in [
            "I am free Monday. 17 November works too.",
            "Montag\n17.11.2026 geht auch",
        ]:
            with self.subTest(text=text):
                self.assertEqual(check.date_findings(text, TODAY), [])
        self.assertEqual(
            len(check.date_findings("Mon. 16 Nov 2026 or Tue. 16 Nov 2026", TODAY)), 1
        )

    def test_the_verb_may_is_no_month(self):
        self.assertEqual(check.date_findings("Monday may 17 people join.", TODAY), [])
        # 17 May 2026 (a Sunday) lies more than 60 days back, so the date is 17 May 2027, a Monday.
        self.assertEqual(check.date_findings("Can we meet Monday May 17?", TODAY), [])
        (f,) = check.date_findings("Can we meet Sunday May 17?", TODAY)
        self.assertIn("17 May 2027 is a Monday", f["detail"])

    def test_29_february_without_a_year_is_not_called_impossible(self):
        self.assertEqual(check.date_findings("Tuesday, 29 February", TODAY), [])
        (f,) = check.date_findings("Dienstag, 30.02.", TODAY)
        self.assertEqual(f["title"], "No such date")

    def test_both_weekdays_next_to_one_date_are_checked(self):
        (f,) = check.date_findings("Tuesday, 17 November 2026 (Monday)", TODAY)
        self.assertEqual(f["quote"], "17 November 2026 (Monday)")

    def test_odd_attachment_entries_and_long_quotes_break_nothing(self):
        long_body = "x" * 5000
        answer = envelope(
            [
                {
                    "level": "warn",
                    "title": "t",
                    "detail": "d",
                    "quote": long_body,
                    "confirm": False,
                }
            ]
        )
        with tempfile.TemporaryDirectory() as d:
            (f,) = check.second_reader(
                view(long_body, attachments=()) | {"attachments": [None, "a.pdf"]},
                fake_claude(d, answer),
                None,
                30,
            )
        self.assertEqual(len(f["quote"]), check.MAX_QUOTE_CHARS)
        self.assertTrue(long_body.startswith(f["quote"]))

    def test_german_so_with_a_comma_is_a_word(self):
        self.assertEqual(
            check.date_findings("So, den 18.11. schicke ich die Rechnung.", TODAY), []
        )
        (f,) = check.date_findings("So., 18.11.", TODAY)
        self.assertIn("Wednesday", f["detail"])

    def test_a_weekday_after_the_date_and_a_comma(self):
        (f,) = check.date_findings("Am 17.11.2026, Montag, bin ich im Büro.", TODAY)
        self.assertEqual(f["quote"], "17.11.2026, Montag")
        self.assertEqual(
            check.date_findings("Am 17.11.2026, Dienstag, bin ich da.", TODAY), []
        )

    def test_a_coming_june_date_takes_next_year(self):
        self.assertEqual(
            check.date_findings("Let us meet Thursday, 17 June", TODAY), []
        )

    def test_anliegend_counts_as_an_attachment(self):
        (f,) = check.attachment_findings("Anliegend die Unterlagen.", [])
        self.assertEqual(f["quote"], "Anliegend")

    def test_the_subject_is_checked_too(self):
        def run(subject, body):
            out = subprocess.run(
                [str(HERE / "check.py"), "--no-claude"],
                input=json.dumps(view(body) | {"subject": subject}),
                capture_output=True,
                text=True,
            )
            return json.loads(out.stdout)["findings"]

        (f,) = run("Monday, 17 November 2026", "Looking forward to it.")
        self.assertEqual((f["level"], f["quote"]), ("block", ""))
        self.assertIn("subject", f["detail"])
        (f,) = run("Angebot im Anhang", "Hallo Ann")
        self.assertEqual((f["level"], f["quote"]), ("warn", ""))
        (f,) = run("Re: Offer", "Monday, 17 November 2026 works.")
        self.assertEqual(f["quote"], "Monday, 17 November 2026")

    def test_utf8_in_and_out_whatever_the_locale(self):
        seen = {}

        def run(argv, **kwargs):
            seen.update(kwargs)
            raise OSError(2, "stop here")

        with mock.patch.object(check.subprocess, "run", run):
            check.second_reader(view("Grüße"), "/bin/false", None, 30)
        self.assertEqual(seen["encoding"], "utf-8")
        out = self.run_utf8()
        self.assertTrue(out.isascii(), out)

    def run_utf8(self):
        out = subprocess.run(
            [str(HERE / "check.py"), "--no-claude"],
            input=json.dumps(view("Anbei die Grüße."), ensure_ascii=False).encode(
                "utf-8"
            ),
            capture_output=True,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        return out.stdout.decode("ascii")


if __name__ == "__main__":
    unittest.main()
