import os
from pathlib import Path
import stat
import subprocess
import tempfile
import textwrap
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
RELOAD_SCRIPT = PROJECT_ROOT / "scripts" / "reload_local.sh"


class ReloadLocalTest(unittest.TestCase):
    def test_reload_rejects_shadow_before_missing_destination_without_mutation(self):
        with tempfile.TemporaryDirectory() as raw_temp_dir:
            temp_dir = Path(raw_temp_dir)
            source = temp_dir / "source-herdr"
            destination = temp_dir / "local-bin" / "herdr"
            shadow_bin = temp_dir / "shadow-bin"
            handoff_log = temp_dir / "handoff.log"
            shadow_bin.mkdir()

            source.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$HERDR_TEST_HANDOFF_LOG"\n',
                encoding="utf-8",
            )
            source.chmod(source.stat().st_mode | stat.S_IXUSR)

            shadow = shadow_bin / "herdr"
            shadow.write_text("#!/bin/sh\necho shadow\n", encoding="utf-8")
            shadow.chmod(shadow.stat().st_mode | stat.S_IXUSR)

            env = os.environ.copy()
            env.update(
                {
                    "HERDR_RELOAD_SOURCE": str(source),
                    "HERDR_RELOAD_DESTINATION": str(destination),
                    "HERDR_RELOAD_SKIP_BUILD": "1",
                    "HERDR_TEST_HANDOFF_LOG": str(handoff_log),
                    "PATH": (
                        f"{shadow_bin}{os.pathsep}{destination.parent}"
                        f"{os.pathsep}{env['PATH']}"
                    ),
                }
            )

            result = subprocess.run(
                [str(RELOAD_SCRIPT)],
                check=False,
                env=env,
                text=True,
                capture_output=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(str(shadow), result.stderr)
            self.assertIn(str(destination), result.stderr)
            self.assertFalse(destination.exists())
            self.assertFalse(handoff_log.exists())

    def test_reload_allows_missing_destination_when_its_directory_will_win(self):
        with tempfile.TemporaryDirectory() as raw_temp_dir:
            temp_dir = Path(raw_temp_dir)
            source = temp_dir / "source-herdr"
            destination = temp_dir / "local-bin" / "herdr"
            shadow_bin = temp_dir / "shadow-bin"
            fake_tools = temp_dir / "tools"
            destination.parent.mkdir()
            shadow_bin.mkdir()
            fake_tools.mkdir()

            source.write_text(
                "#!/bin/sh\n"
                'if [ "${1:-}" = "--version" ]; then echo "herdr test"; fi\n',
                encoding="utf-8",
            )
            source.chmod(source.stat().st_mode | stat.S_IXUSR)

            shadow = shadow_bin / "herdr"
            shadow.write_text("#!/bin/sh\necho shadow\n", encoding="utf-8")
            shadow.chmod(shadow.stat().st_mode | stat.S_IXUSR)

            fake_codesign = fake_tools / "codesign"
            fake_codesign.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codesign.chmod(fake_codesign.stat().st_mode | stat.S_IXUSR)

            env = os.environ.copy()
            env.update(
                {
                    "HERDR_RELOAD_SOURCE": str(source),
                    "HERDR_RELOAD_DESTINATION": str(destination),
                    "HERDR_RELOAD_SKIP_BUILD": "1",
                    "HERDR_RELOAD_SKIP_HANDOFF": "1",
                    "PATH": (
                        f"{destination.parent}{os.pathsep}{shadow_bin}"
                        f"{os.pathsep}{fake_tools}{os.pathsep}{env['PATH']}"
                    ),
                }
            )

            subprocess.run([str(RELOAD_SCRIPT)], check=True, env=env, text=True)

            self.assertEqual(destination.read_bytes(), source.read_bytes())

    def test_replaces_destination_inode_and_hands_off_to_installed_binary(self):
        with tempfile.TemporaryDirectory() as raw_temp_dir:
            temp_dir = Path(raw_temp_dir)
            source = temp_dir / "source-herdr"
            destination = temp_dir / "bin" / "herdr"
            handoff_log = temp_dir / "handoff.log"
            fake_tools = temp_dir / "tools"
            fake_tools.mkdir()
            destination.parent.mkdir()

            source.write_text(
                textwrap.dedent(
                    """\
                    #!/bin/sh
                    if [ "${1:-}" = "--version" ]; then
                        echo "herdr test"
                        exit 0
                    fi
                    printf '%s\\n' "$*" >> "$HERDR_TEST_HANDOFF_LOG"
                    """
                ),
                encoding="utf-8",
            )
            source.chmod(source.stat().st_mode | stat.S_IXUSR)
            destination.write_text("old executable\n", encoding="utf-8")
            destination.chmod(destination.stat().st_mode | stat.S_IXUSR)
            old_inode = destination.stat().st_ino

            fake_codesign = fake_tools / "codesign"
            fake_codesign.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codesign.chmod(fake_codesign.stat().st_mode | stat.S_IXUSR)

            env = os.environ.copy()
            env.update(
                {
                    "HERDR_RELOAD_SOURCE": str(source),
                    "HERDR_RELOAD_DESTINATION": str(destination),
                    "HERDR_RELOAD_SKIP_BUILD": "1",
                    "HERDR_TEST_HANDOFF_LOG": str(handoff_log),
                    "PATH": (
                        f"{destination.parent}{os.pathsep}{fake_tools}"
                        f"{os.pathsep}{env['PATH']}"
                    ),
                }
            )

            subprocess.run([str(RELOAD_SCRIPT)], check=True, env=env, text=True)

            self.assertNotEqual(destination.stat().st_ino, old_inode)
            self.assertEqual(destination.read_bytes(), source.read_bytes())
            self.assertEqual(
                handoff_log.read_text(encoding="utf-8").strip(),
                f"server live-handoff --import-exe {destination}",
            )

    def test_check_mode_rejects_shadowed_destination_without_mutation(self):
        with tempfile.TemporaryDirectory() as raw_temp_dir:
            temp_dir = Path(raw_temp_dir)
            source = temp_dir / "source-herdr"
            destination = temp_dir / "local-bin" / "herdr"
            shadow_bin = temp_dir / "shadow-bin"
            fake_tools = temp_dir / "tools"
            destination.parent.mkdir()
            shadow_bin.mkdir()
            fake_tools.mkdir()

            source.write_text("#!/bin/sh\necho source\n", encoding="utf-8")
            source.chmod(source.stat().st_mode | stat.S_IXUSR)
            destination.write_text("#!/bin/sh\necho destination\n", encoding="utf-8")
            destination.chmod(destination.stat().st_mode | stat.S_IXUSR)
            destination_before = destination.read_bytes()

            shadow = shadow_bin / "herdr"
            shadow.write_text("#!/bin/sh\necho shadow\n", encoding="utf-8")
            shadow.chmod(shadow.stat().st_mode | stat.S_IXUSR)

            fake_codesign = fake_tools / "codesign"
            fake_codesign.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codesign.chmod(fake_codesign.stat().st_mode | stat.S_IXUSR)

            env = os.environ.copy()
            env.update(
                {
                    "HERDR_RELOAD_SOURCE": str(source),
                    "HERDR_RELOAD_DESTINATION": str(destination),
                    "HERDR_RELOAD_SKIP_BUILD": "1",
                    "HERDR_RELOAD_SKIP_HANDOFF": "1",
                    "PATH": (
                        f"{shadow_bin}{os.pathsep}{destination.parent}"
                        f"{os.pathsep}{fake_tools}{os.pathsep}{env['PATH']}"
                    ),
                }
            )

            result = subprocess.run(
                [str(RELOAD_SCRIPT), "--check"],
                check=False,
                env=env,
                text=True,
                capture_output=True,
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(str(shadow), result.stderr)
            self.assertIn(str(destination), result.stderr)
            self.assertEqual(destination.read_bytes(), destination_before)

    def test_check_mode_reports_compatible_destination_without_mutation(self):
        with tempfile.TemporaryDirectory() as raw_temp_dir:
            temp_dir = Path(raw_temp_dir)
            source = temp_dir / "source-herdr"
            destination = temp_dir / "bin" / "herdr"
            fake_tools = temp_dir / "tools"
            destination.parent.mkdir()
            fake_tools.mkdir()

            source.write_text("#!/bin/sh\necho source\n", encoding="utf-8")
            source.chmod(source.stat().st_mode | stat.S_IXUSR)
            destination.write_text(
                textwrap.dedent(
                    """\
                    #!/bin/sh
                    case "${1:-} ${2:-}" in
                        "--version ") echo "herdr test" ;;
                        "status --json") echo '{"client":{"protocol":18},"server":{"protocol":18,"compatible":true}}' ;;
                        *) exit 2 ;;
                    esac
                    """
                ),
                encoding="utf-8",
            )
            destination.chmod(destination.stat().st_mode | stat.S_IXUSR)
            destination_before = destination.read_bytes()

            fake_codesign = fake_tools / "codesign"
            fake_codesign.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codesign.chmod(fake_codesign.stat().st_mode | stat.S_IXUSR)

            env = os.environ.copy()
            env.update(
                {
                    "HERDR_RELOAD_SOURCE": str(source),
                    "HERDR_RELOAD_DESTINATION": str(destination),
                    "HERDR_RELOAD_SKIP_BUILD": "1",
                    "HERDR_RELOAD_SKIP_HANDOFF": "1",
                    "PATH": (
                        f"{destination.parent}{os.pathsep}{fake_tools}"
                        f"{os.pathsep}{env['PATH']}"
                    ),
                }
            )

            result = subprocess.run(
                [str(RELOAD_SCRIPT), "--check"],
                check=False,
                env=env,
                text=True,
                capture_output=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(str(destination), result.stdout)
            self.assertIn('"compatible":true', result.stdout)
            self.assertEqual(destination.read_bytes(), destination_before)


if __name__ == "__main__":
    unittest.main()
