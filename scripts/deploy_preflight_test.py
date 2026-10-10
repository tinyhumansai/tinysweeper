"""Exercise deployment's remote shell against a fake Compose command."""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DeploymentPreflightTests(unittest.TestCase):
    def deploy(self, validation_exit):
        workflow = (ROOT / '.github/workflows/deploy.yml').read_text()
        script = textwrap.dedent(workflow.split("<<'REMOTE'\n", 1)[1].split('\n          REMOTE', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.env').write_text('')
            log = root / 'commands'
            docker = root / 'docker'
            docker.write_text('''#!/usr/bin/env python3
import os, sys
with open(os.environ['DEPLOY_TEST_LOG'], 'a') as log:
    log.write(' '.join(sys.argv[1:]) + '\\n')
if 'run' in sys.argv and 'check' in sys.argv:
    sys.exit(int(os.environ['DEPLOY_TEST_VALIDATION_EXIT']))
''')
            docker.chmod(0o755)
            env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                       DEPLOY_TEST_LOG=str(log),
                       DEPLOY_TEST_VALIDATION_EXIT=str(validation_exit))
            result = subprocess.run(['bash', '-s', '--', str(root), 'sha-test'],
                                    input=script, text=True, env=env,
                                    capture_output=True, check=False)
            return result.returncode, log.read_text().splitlines()

    def test_invalid_mounted_configuration_keeps_running_service(self):
        code, commands = self.deploy(1)
        self.assertNotEqual(code, 0)
        self.assertTrue(any(' run ' in command and ' check ' in command for command in commands))
        self.assertFalse(any(' up ' in command for command in commands))

    def test_valid_configuration_is_checked_with_new_image_before_recreate(self):
        code, commands = self.deploy(0)
        self.assertEqual(code, 0)
        pull = next(i for i, command in enumerate(commands) if ' pull ' in command)
        check = next(i for i, command in enumerate(commands) if ' run ' in command and ' check ' in command)
        up = next(i for i, command in enumerate(commands) if ' up ' in command)
        self.assertLess(pull, check)
        self.assertLess(check, up)
        self.assertIn('--no-deps', commands[check])
        self.assertIn('--rm', commands[check])
        self.assertIn('--interactive=false', commands[check])


if __name__ == '__main__':
    unittest.main()
