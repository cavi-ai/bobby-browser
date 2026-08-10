import os
import pathlib
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from providers import DEFAULT_PROVIDER, create_provider
from providers.ollama_provider import OllamaProvider


class ProviderRegistryTests(unittest.TestCase):
    def test_portable_default_is_ollama(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            provider = create_provider()

        self.assertEqual(DEFAULT_PROVIDER, "ollama")
        self.assertIsInstance(provider, OllamaProvider)


if __name__ == "__main__":
    unittest.main()
