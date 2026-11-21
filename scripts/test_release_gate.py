import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location('release_gate', pathlib.Path(__file__).with_name('release-gate.py'))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class ReleaseGateTest(unittest.TestCase):
    def test_ci_exact_identity_and_latest_attempt(self):
        run = dict(event='push', head_branch='v0.27.0', head_sha='a' * 40, run_number=1, run_attempt=1, status='completed', conclusion='success', id=1)
        jobs = lambda _: [dict(name=n, conclusion='success') for n in gate.REQUIRED_JOBS]
        self.assertEqual(gate.ci_result([run], 'v0.27.0', 'a' * 40, jobs), 'success')
        self.assertEqual(gate.ci_result([run], 'v0.27.0', 'b' * 40, jobs), 'failed')
        self.assertEqual(gate.ci_result([run], 'v0.27.0', 'a' * 40, lambda _: []), 'failed')
        failed = dict(run, run_number=2, conclusion='failure')
        self.assertEqual(gate.ci_result([run, failed], 'v0.27.0', 'a' * 40, jobs), 'failed')

    def test_qualified_tag_peeling_and_checkout_identity(self):
        paths = {
            'repos/owner/repo/git/ref/tags/v0.27.0': {'object': {'type': 'tag', 'sha': 'tag-object'}},
            'repos/owner/repo/git/tags/tag-object': {'object': {'type': 'commit', 'sha': 'a' * 40}},
        }
        sha = gate.tag_commit('owner/repo', 'v0.27.0', paths.__getitem__)
        gate.require_checkout('v0.27.0', sha, 'a' * 40)
        with self.assertRaises(ValueError):
            gate.require_checkout('v0.27.0', sha, 'b' * 40)

    def test_assets_must_be_complete_and_published(self):
        tag = 'v0.27.0'
        names = [f'bobby-browser-0.27.0-{p}.{e}' for p, e in [('linux-x64', 'tar.gz'), ('linux-arm64', 'tar.gz'), ('macos-arm64', 'tar.gz'), ('macos-x64', 'tar.gz'), ('windows-x64', 'zip')]]
        names += [f'bobby-browser-docs-{tag}.tar.gz', f'bobby-browser-docs-{tag}.tar.gz.sha256']
        release = dict(draft=False, prerelease=False, tag_name=tag, assets=[dict(name=n, state='uploaded') for n in names])
        self.assertTrue(gate.check_assets(release, tag))
        self.assertFalse(gate.check_assets(dict(release, draft=True), tag))
        self.assertFalse(gate.check_assets(dict(release, assets=release['assets'][:-1]), tag))


if __name__ == '__main__':
    unittest.main()
