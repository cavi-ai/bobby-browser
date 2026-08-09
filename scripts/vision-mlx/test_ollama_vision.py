import importlib.util
import pathlib
import unittest


MODULE_PATH = pathlib.Path(__file__).with_name("ollama_vision.py")
SPEC = importlib.util.spec_from_file_location("ollama_vision", MODULE_PATH)
ollama_vision = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(ollama_vision)


class OllamaVisionTests(unittest.TestCase):
    def test_config_loads_without_starting_the_server(self):
        config = ollama_vision.Config()

        self.assertEqual(config.bind, "127.0.0.1:9103")
        self.assertEqual(config.model_name, "llava:7b")

    def test_normalize_response_extracts_a_bobby_click(self):
        client = ollama_vision.OllamaClient("llava:7b", "http://127.0.0.1:11434")

        proposal = client._normalize_response(
            'Here is the result: {"confidence": 0.92, "action": {"x": 42, "y": 17}}'
        )

        self.assertEqual(
            proposal,
            {
                "confidence": 0.92,
                "action": {"kind": "click", "x": 42, "y": 17},
            },
        )

    def test_normalize_response_strips_unknown_model_fields(self):
        client = ollama_vision.OllamaClient("llava:7b", "http://127.0.0.1:11434")

        proposal = client._normalize_response(
            '{"confidence": 0.9, "reasoning": "visible", '
            '"action": {"kind": "click", "x": 12, "y": 34, "label": "Continue"}}'
        )

        self.assertEqual(
            proposal,
            {
                "confidence": 0.9,
                "action": {"kind": "click", "x": 12.0, "y": 34.0},
            },
        )

    def test_normalize_response_fails_closed_for_valid_but_wrong_json_shape(self):
        client = ollama_vision.OllamaClient("llava:7b", "http://127.0.0.1:11434")

        proposal = client._normalize_response('["not", "a", "proposal"]')

        self.assertEqual(
            proposal,
            {
                "confidence": 0.0,
                "action": {"kind": "click", "x": 0.0, "y": 0.0},
            },
        )


if __name__ == "__main__":
    unittest.main()
