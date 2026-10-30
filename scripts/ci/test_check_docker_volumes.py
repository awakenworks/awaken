"""Contract tests for check_docker_volumes.py: bad samples fail, good ones pass."""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from check_docker_volumes import check_text  # noqa: E402


class CheckDockerVolumesTest(unittest.TestCase):
    def violations(self, script):
        return check_text(script, "sample.sh")

    def test_bare_postgres_run_is_a_violation(self):
        self.assertTrue(self.violations('docker run -d --name pg -e POSTGRES_PASSWORD=x postgres:16-alpine'))

    def test_postgres_with_tmpfs_or_named_volume_passes(self):
        self.assertEqual(self.violations('docker run -d --tmpfs /var/lib/postgresql/data postgres:16-alpine'), [])
        self.assertEqual(self.violations('docker run -d -v "$vol:/var/lib/postgresql/data" postgres:16'), [])
        self.assertEqual(self.violations('docker run -d --tmpfs /var/lib/postgresql postgres:18-alpine'), [])

    def test_unrelated_mount_does_not_cover_the_data_directory(self):
        self.assertTrue(self.violations('docker run -d -v /host/wal:/wal-archive postgres:16'))

    def test_foreground_rm_run_is_allowed_but_detached_rm_is_not(self):
        self.assertEqual(self.violations('docker run --rm --network host postgres:16-alpine psql -c x'), [])
        self.assertTrue(self.violations('docker run -d --rm postgres:16-alpine'))

    def test_image_resolved_through_a_script_variable(self):
        script = 'PG_IMAGE="postgres:16-alpine"\ndocker run -d --name pg "$PG_IMAGE"\n'
        self.assertTrue(self.violations(script))

    def test_multiline_command_substitution(self):
        script = 'id="$(docker run --detach --publish-all \\\n  --env A=b \\\n  postgres:16-alpine)"\n'
        self.assertTrue(self.violations(script))

    def test_other_stateful_images(self):
        for image in ("minio/minio:RELEASE.1", "registry:2", "moby/buildkit:v0.30.0", "hashicorp/vault:1.17.6"):
            self.assertTrue(self.violations(f"docker run -d {image}"), image)

    def test_image_without_volume_is_ignored(self):
        self.assertEqual(self.violations('docker run -d --name postgres-proxy busybox:1.37'), [])
        self.assertEqual(self.violations('docker run -d --name postgres busybox:1.37'), [])

    def test_docker_rm_needs_volumes_flag(self):
        self.assertTrue(self.violations('docker rm -f "$name"'))
        self.assertTrue(self.violations('docker rm "$name"'))
        self.assertTrue(self.violations('docker ps -aq | xargs docker rm -f'))
        for ok in ('docker rm -fv n', 'docker rm -f -v n', 'docker rm --force --volumes n', 'docker rm -v n'):
            self.assertEqual(self.violations(ok), [], ok)

    def test_volume_and_image_removal_are_not_container_removal(self):
        self.assertEqual(self.violations('docker volume rm v; docker image rm i; docker rmi i'), [])

    def test_allow_marker_and_comments(self):
        self.assertEqual(self.violations('# docker rm -f x\n'), [])
        self.assertEqual(self.violations('docker rm -f x  # docker-volume-lint: allow no volumes\n'), [])
        self.assertEqual(self.violations('# docker-volume-lint: allow scratch image\ndocker rm x\n'), [])


if __name__ == "__main__":
    unittest.main()
