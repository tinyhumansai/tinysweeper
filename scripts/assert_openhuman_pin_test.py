"""Offline fixtures for the source pin and checked-out dependency contract."""

import pathlib
import shutil
import subprocess
import tempfile
import unittest


def git(root, *arguments):
    return subprocess.check_output(
        ["git", "-c", "user.name=Pin Test", "-c", "user.email=pin@example.invalid", *arguments],
        cwd=root,
        text=True,
        stderr=subprocess.DEVNULL,
    ).strip()


class SourcePinTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="openhuman-pin-")
        self.addCleanup(self.fixture.cleanup)
        self.root = pathlib.Path(self.fixture.name)
        self.vendor = self.root / "vendor/openhuman"
        self.vendor.mkdir(parents=True)
        git(self.vendor, "init", "-q")
        (self.vendor / "Cargo.toml").write_text("[workspace]\n")
        git(self.vendor, "add", "Cargo.toml")
        git(self.vendor, "commit", "-qm", "source")
        self.pin = git(self.vendor, "rev-parse", "HEAD")
        git(self.root, "init", "-q")
        (self.root / "Cargo.toml").write_text(
            f'[package.metadata.openhuman]\nrev = "{self.pin}"\n'
        )
        (self.root / "scripts").mkdir()
        shutil.copyfile(
            pathlib.Path(__file__).with_name("assert-openhuman-pin.sh"),
            self.root / "scripts/assert-openhuman-pin.sh",
        )
        git(self.root, "add", "Cargo.toml", "scripts")
        git(self.root, "update-index", "--add", "--cacheinfo", f"160000,{self.pin},vendor/openhuman")
        git(self.root, "commit", "-qm", "consumer")

    def check_pin(self):
        return subprocess.run(
            ["sh", "scripts/assert-openhuman-pin.sh"],
            cwd=self.root,
            capture_output=True,
            text=True,
        )

    def test_matching_recorded_and_checked_out_sources_pass(self):
        result = self.check_pin()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_changed_checkout_cannot_pass_using_the_old_recorded_pointer(self):
        git(self.vendor, "commit", "--allow-empty", "-qm", "different source")
        result = self.check_pin()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checked out", result.stderr)

    def test_a_changed_recorded_pointer_is_rejected(self):
        git(self.vendor, "commit", "--allow-empty", "-qm", "different source")
        other = git(self.vendor, "rev-parse", "HEAD")
        git(self.root, "update-index", "--cacheinfo", f"160000,{other},vendor/openhuman")
        git(self.root, "commit", "-qm", "different pointer")
        git(self.vendor, "checkout", "-q", self.pin)
        result = self.check_pin()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("records", result.stderr)


if __name__ == "__main__":
    unittest.main()
