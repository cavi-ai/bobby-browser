#!/usr/bin/env python3
"""
Bobby Vision Model Fine-Tuning Pipeline

Fine-tunes vision-language models on Bobby's specific automation tasks.
Uses Ollama (llava:7b) for inference, with a path to MLX fine-tuning
when vision support lands.

Usage:
    python fine_tune_vision.py --input data/training_data.jsonl --output models/
    python fine_tune_vision.py --ollama --model llava:7b --input data/training_data.jsonl
"""

import argparse
import base64
import io
import json
import os
import sys
import time
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Optional

import numpy as np
import requests
from PIL import Image


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclass
class FineTuneConfig:
    """Fine-tuning configuration."""
    model_name: str = "llava:7b"
    input_path: str = "data/training_data.jsonl"
    output_dir: str = "models"
    learning_rate: float = 2e-5
    num_epochs: int = 3
    batch_size: int = 4
    lora_rank: int = 16
    lora_alpha: float = 32.0
    lora_dropout: float = 0.05
    max_image_size: int = 1024
    max_text_tokens: int = 512
    seed: int = 42
    ollama_base_url: str = "http://127.0.0.1:11434"
    use_ollama: bool = False  # Use Ollama for inference (default)
    use_mlx: bool = False  # Use MLX for fine-tuning (future)


# ---------------------------------------------------------------------------
# Data Processing
# ---------------------------------------------------------------------------

class TrainingDataProcessor:
    """Processes raw training data into model-ready format."""

    def __init__(self, config: FineTuneConfig):
        self.config = config

    def load_dataset(self, path: str) -> list:
        """Load JSONL training data."""
        examples = []
        with open(path, "r") as f:
            for line in f:
                if line.strip():
                    examples.append(json.loads(line))
        print(f"Loaded {len(examples)} training examples")
        return examples

    def preprocess_image(self, image_b64: str) -> np.ndarray:
        """Preprocess image for model input."""
        image_bytes = base64.b64decode(image_b64)
        image = Image.open(io.BytesIO(image_bytes)).convert("RGB")

        max_size = self.config.max_image_size
        if image.width > max_size or image.height > max_size:
            image.thumbnail((max_size, max_size))

        img_array = np.array(image).astype(np.float32) / 255.0
        return img_array

    def build_prompt(self, example: dict) -> str:
        """Build training prompt from example."""
        system = (
            "You are a vision assistant for a browser automation agent called Bobby. "
            "Analyze the screenshot and return ONLY valid JSON matching this schema: "
            '{"confidence": 0.0..1.0, "action": {"kind": "click" | "typeText" | "extractValue", ...}}. '
            "Click coordinates are CSS pixels relative to the screenshot image."
        )

        user_text = f"purpose: {example['purpose']}\nintentKind: {example['intent_kind']}\nstuck: {example['stuck']}"

        if example.get("context_url"):
            user_text += f"\nurl: {example['context_url']}"
        if example.get("context_candidates"):
            user_text += "\ncandidates:"
            for c in example["context_candidates"]:
                user_text += f"\n- {c['role']} \"{c['name']}\""

        return f"{system}\n\n{user_text}"

    def build_target(self, example: dict) -> str:
        """Build training target (expected output)."""
        action = example.get("model_response", {}).get("action", {})
        confidence = example.get("model_response", {}).get("confidence", 0.5)

        if action.get("kind") == "click":
            return json.dumps({
                "confidence": confidence,
                "action": {
                    "kind": "click",
                    "x": action.get("x", 0.0),
                    "y": action.get("y", 0.0),
                },
            })
        elif action.get("kind") == "typeText":
            return json.dumps({
                "confidence": confidence,
                "action": {
                    "kind": "typeText",
                    "text": action.get("text", ""),
                },
            })
        elif action.get("kind") == "extractValue":
            return json.dumps({
                "confidence": confidence,
                "action": {
                    "kind": "extractValue",
                    "value": action.get("value", ""),
                },
            })
        else:
            return json.dumps({
                "confidence": 0.5,
                "action": {"kind": "click", "x": 0.0, "y": 0.0},
            })

    def prepare_training_data(self, examples: list) -> list:
        """Prepare data for fine-tuning."""
        processed = []
        for example in examples:
            processed.append({
                "image": self.preprocess_image(example["image_b64"]),
                "prompt": self.build_prompt(example),
                "target": self.build_target(example),
                "success": example.get("success", False),
                "journey": example.get("journey", ""),
                "step": example.get("step", ""),
            })
        return processed

    def split_dataset(self, data: list, train_ratio: float = 0.8) -> tuple:
        """Split data into train/validation sets."""
        np.random.seed(self.config.seed)
        indices = np.random.permutation(len(data))
        split = int(len(data) * train_ratio)

        train = [data[i] for i in indices[:split]]
        val = [data[i] for i in indices[split:]]

        print(f"Train: {len(train)}, Validation: {len(val)}")
        return train, val


# ---------------------------------------------------------------------------
# Ollama Inference (current working path)
# ---------------------------------------------------------------------------

class OllamaInferenceEngine:
    """Runs vision model inference via Ollama API."""

    def __init__(self, config: FineTuneConfig):
        self.config = config
        self.base_url = config.ollama_base_url.rstrip("/")
        self.processor = TrainingDataProcessor(config)

    def predict(self, image_b64: str, prompt: str) -> dict:
        """Run inference via Ollama and return prediction."""
        user_content = [
            {"type": "text", "text": prompt},
            {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{image_b64}"}},
        ]

        body = {
            "model": self.config.model_name,
            "messages": [
                {"role": "system", "content": "You are a vision assistant for a browser automation agent. Return only valid JSON."},
                {"role": "user", "content": user_content},
            ],
        }

        response = requests.post(
            f"{self.base_url}/v1/chat/completions",
            json=body,
            timeout=60,
        )
        response.raise_for_status()

        result = response.json()
        content = result["choices"][0]["message"]["content"]

        # Strip markdown fences
        content = content.strip()
        if content.startswith("```json"):
            content = content[7:]
        if content.startswith("```"):
            content = content[3:]
        content = content.strip().rstrip("```").strip()

        return json.loads(content)

    def run_inference_on_dataset(self, raw_examples: list) -> list:
        """Run inference on all examples and return predictions."""
        print(f"\nRunning Ollama inference on {len(raw_examples)} examples...")
        predictions = []

        for i, example in enumerate(raw_examples):
            start = time.time()
            try:
                prompt = self.processor.build_prompt(example)
                prediction = self.predict(example["image_b64"], prompt)
                elapsed = time.time() - start
                predictions.append({
                    "example_idx": i,
                    "journey": example.get("journey", ""),
                    "step": example.get("step", ""),
                    "success": example.get("success", False),
                    "prediction": prediction,
                    "target": self.processor.build_target(example),
                    "elapsed": elapsed,
                })
                if (i + 1) % 10 == 0:
                    print(f"  Processed {i + 1}/{len(raw_examples)} ({elapsed:.1f}s avg)")
            except Exception as e:
                print(f"  Error on example {i}: {e}")
                predictions.append({
                    "example_idx": i,
                    "journey": example.get("journey", ""),
                    "step": example.get("step", ""),
                    "success": example.get("success", False),
                    "prediction": None,
                    "target": self.processor.build_target(example),
                    "error": str(e),
                    "elapsed": 0,
                })

        print(f"\nInference complete. {len(predictions)} examples processed.")
        return predictions


# ---------------------------------------------------------------------------
# Evaluation
# ---------------------------------------------------------------------------

class VisionEvaluator:
    """Evaluates model predictions against ground truth."""

    def __init__(self, config: FineTuneConfig):
        self.config = config

    def evaluate_predictions(self, predictions: list) -> dict:
        """Evaluate model predictions."""
        total = len(predictions)
        successful = sum(1 for p in predictions if p.get("prediction") is not None)
        failed = total - successful

        # Action accuracy
        action_correct = 0
        coord_within_10px = 0
        coord_within_50px = 0
        total_with_coordinates = 0

        for p in predictions:
            pred = p.get("prediction")
            if pred is None:
                continue

            pred_action = pred.get("action", {})
            target = json.loads(p["target"])
            target_action = target.get("action", {})

            # Action type accuracy
            if pred_action.get("kind") == target_action.get("kind"):
                action_correct += 1

            # Coordinate accuracy (for click actions)
            if pred_action.get("kind") == "click" and target_action.get("kind") == "click":
                total_with_coordinates += 1
                pred_x = pred_action.get("x", 0.0)
                pred_y = pred_action.get("y", 0.0)
                target_x = target_action.get("x", 0.0)
                target_y = target_action.get("y", 0.0)
                dist = ((pred_x - target_x) ** 2 + (pred_y - target_y) ** 2) ** 0.5

                if dist < 10:
                    coord_within_10px += 1
                if dist < 50:
                    coord_within_50px += 1

        action_accuracy = action_correct / successful if successful > 0 else 0
        coord_mae = 0.0
        if total_with_coordinates > 0:
            dists = []
            for p in predictions:
                pred = p.get("prediction")
                if pred is None:
                    continue
                pred_action = pred.get("action", {})
                target = json.loads(p["target"])
                target_action = target.get("action", {})
                if pred_action.get("kind") == "click" and target_action.get("kind") == "click":
                    dists.append(
                        ((pred_action.get("x", 0) - target_action.get("x", 0)) ** 2 +
                         (pred_action.get("y", 0) - target_action.get("y", 0)) ** 2) ** 0.5
                    )
            coord_mae = np.mean(dists) if dists else 0.0

        # Journey-level success rate
        journey_stats = {}
        for p in predictions:
            journey = p.get("journey", "unknown")
            if journey not in journey_stats:
                journey_stats[journey] = {"total": 0, "correct": 0}
            journey_stats[journey]["total"] += 1
            if p.get("prediction") is not None:
                pred = p["prediction"]
                target = json.loads(p["target"])
                if pred.get("action", {}).get("kind") == target.get("action", {}).get("kind"):
                    journey_stats[journey]["correct"] += 1

        # Confidence calibration
        confidences = []
        calibrations = []
        for p in predictions:
            pred = p.get("prediction")
            if pred is None:
                continue
            conf = pred.get("confidence", 0.5)
            was_correct = (pred.get("action", {}).get("kind") ==
                          json.loads(p["target"]).get("action", {}).get("kind"))
            confidences.append(conf)
            calibrations.append(1.0 if was_correct else 0.0)

        # Journey success rates
        journey_success_rates = {}
        for journey, stats in journey_stats.items():
            journey_success_rates[journey] = stats["correct"] / stats["total"] if stats["total"] > 0 else 0

        return {
            "total_examples": total,
            "successful_predictions": successful,
            "failed_predictions": failed,
            "action_accuracy": action_accuracy,
            "coord_accuracy_10px": coord_within_10px / total_with_coordinates if total_with_coordinates > 0 else 0,
            "coord_accuracy_50px": coord_within_50px / total_with_coordinates if total_with_coordinates > 0 else 0,
            "coord_mae": coord_mae,
            "journey_success_rates": journey_success_rates,
            "avg_confidence": np.mean(confidences) if confidences else 0,
            "avg_confidence_calibration": np.mean(calibrations) if calibrations else 0,
        }


# ---------------------------------------------------------------------------
# Fine-Tuning (placeholder for MLX when vision support lands)
# ---------------------------------------------------------------------------

class VisionFineTuner:
    """Fine-tunes vision-language models."""

    def __init__(self, config: FineTuneConfig):
        self.config = config
        self.processor = TrainingDataProcessor(config)

    def fine_tune(self):
        """Run fine-tuning pipeline."""
        print(f"Loading model: {self.config.model_name}")

        examples = self.processor.load_dataset(self.config.input_path)
        processed = self.processor.prepare_training_data(examples)
        train, val = self.processor.split_dataset(processed)

        print("\nStarting fine-tuning...")
        self._run_training_loop(train, val)
        self._save_model()

    def _run_training_loop(self, train: list, val: list):
        """Run training loop (placeholder for MLX when vision support lands)."""
        print(f"Training for {self.config.num_epochs} epochs...")
        print("Note: MLX vision support not yet available.")
        print("Use --ollama to run inference on training data instead.")

    def _save_model(self):
        """Save fine-tuned model (or metadata for future fine-tuning)."""
        output_path = Path(self.config.output_dir) / f"{self.config.model_name}-bobby-ft"
        output_path.mkdir(parents=True, exist_ok=True)

        metadata = {
            "model_name": self.config.model_name,
            "training_data": self.config.input_path,
            "learning_rate": self.config.learning_rate,
            "num_epochs": self.config.num_epochs,
            "lora_rank": self.config.lora_rank,
            "lora_alpha": self.config.lora_alpha,
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "mlx_vision_available": False,
            "note": "MLX vision support not yet available. Use --ollama for inference.",
        }

        (output_path / "metadata.json").write_text(json.dumps(metadata, indent=2))
        print(f"\nModel metadata saved to: {output_path}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Bobby Vision Model Fine-Tuning")
    parser.add_argument("--model", default="llava:7b", help="Base model name")
    parser.add_argument("--input", default="data/training_data.jsonl", help="Training data path")
    parser.add_argument("--output", default="models", help="Output directory")
    parser.add_argument("--ollama", action="store_true", help="Use Ollama for inference")
    parser.add_argument("--ollama-base-url", default="http://127.0.0.1:11434", help="Ollama URL")
    parser.add_argument("--epochs", type=int, default=3, help="Number of epochs")
    parser.add_argument("--lr", type=float, default=2e-5, help="Learning rate")
    parser.add_argument("--lora-rank", type=int, default=16, help="LoRA rank")
    parser.add_argument("--batch-size", type=int, default=4, help="Batch size")
    parser.add_argument("--eval", action="store_true", help="Run evaluation")
    parser.add_argument("--predictions", default=None, help="Path to predictions file (for eval)")
    args = parser.parse_args()

    config = FineTuneConfig(
        model_name=args.model,
        input_path=args.input,
        output_dir=args.output,
        learning_rate=args.lr,
        num_epochs=args.epochs,
        batch_size=args.batch_size,
        lora_rank=args.lora_rank,
        use_ollama=args.ollama,
        ollama_base_url=args.ollama_base_url,
    )

    processor = TrainingDataProcessor(config)
    examples = processor.load_dataset(config.input_path)
    processed = processor.prepare_training_data(examples)

    if args.ollama:
        # Run Ollama inference on training data (pass raw examples)
        engine = OllamaInferenceEngine(config)
        predictions = engine.run_inference_on_dataset(examples)

        # Save predictions
        pred_path = Path(config.output_dir) / "ollama_predictions.jsonl"
        pred_path.parent.mkdir(parents=True, exist_ok=True)
        with open(pred_path, "w") as f:
            for p in predictions:
                f.write(json.dumps(p) + "\n")
        print(f"\nPredictions saved to: {pred_path}")

        # Evaluate
        evaluator = VisionEvaluator(config)
        results = evaluator.evaluate_predictions(predictions)

        print("\n=== Evaluation Results ===")
        print(f"Total examples: {results['total_examples']}")
        print(f"Successful predictions: {results['successful_predictions']}")
        print(f"Action accuracy: {results['action_accuracy']:.2%}")
        print(f"Coordinate accuracy (within 10px): {results['coord_accuracy_10px']:.2%}")
        print(f"Coordinate accuracy (within 50px): {results['coord_accuracy_50px']:.2%}")
        print(f"Coordinate MAE: {results['coord_mae']:.2f}")
        print(f"Avg confidence: {results['avg_confidence']:.4f}")
        print(f"Confidence calibration: {results['avg_confidence_calibration']:.4f}")
        print("\nJourney success rates:")
        for journey, rate in results['journey_success_rates'].items():
            print(f"  {journey}: {rate:.2%}")

        # Save results
        results_path = Path(config.output_dir) / "evaluation_results.json"
        results_path.parent.mkdir(parents=True, exist_ok=True)
        results_path.write_text(json.dumps(results, indent=2))
        print(f"\nResults saved to: {results_path}")

    elif args.eval and args.predictions:
        # Evaluate existing predictions
        with open(args.predictions, "r") as f:
            predictions = [json.loads(line) for line in f if line.strip()]

        evaluator = VisionEvaluator(config)
        results = evaluator.evaluate_predictions(predictions)

        print("\n=== Evaluation Results ===")
        print(f"Total examples: {results['total_examples']}")
        print(f"Action accuracy: {results['action_accuracy']:.2%}")
        print(f"Coordinate MAE: {results['coord_mae']:.2f}")

    else:
        # Fine-tune (placeholder)
        tuner = VisionFineTuner(config)
        tuner.fine_tune()


if __name__ == "__main__":
    main()
