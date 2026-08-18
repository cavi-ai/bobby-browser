import os
import pathlib
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))

from providers import DEFAULT_PROVIDER, create_provider
from providers.ollama_provider import OllamaProvider
from providers.mlx_vlm_provider import MlxVlmProvider


class ProviderRegistryTests(unittest.TestCase):
    def test_default_is_mlx_vlm(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            provider = create_provider()

        self.assertEqual(DEFAULT_PROVIDER, "mlx-vlm")
        self.assertIsInstance(provider, MlxVlmProvider)

    def test_explicit_mlx_model_override_reaches_provider(self):
        with mock.patch.dict(os.environ, {"VISION_MLX_MODEL": "env-model"}, clear=True):
            provider = create_provider("mlx-vlm", model="requested-model")

        self.assertIsInstance(provider, MlxVlmProvider)
        self.assertEqual(provider.model_name, "requested-model")


if __name__ == "__main__":
    unittest.main()
