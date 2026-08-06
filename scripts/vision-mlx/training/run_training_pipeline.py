#!/usr/bin/env python3
"""
Bobby Vision Training Pipeline

Complete pipeline: collect data, fine-tune, evaluate.

Usage:
    python run_training_pipeline.py --journey all
    python run_training_pipeline.py --collect --fine-tune --evaluate
"""

import argparse
import json
import os
import sys
from pathlib import Path

# Add scripts directory to path
sys.path.insert(0, str(Path(__file__).parent))

from collect_training_data import (
    VisionDataCollector,
    TrainingDataset,
    VisionTrainingExample,
)
from fine_tune_vision import (
    FineTuneConfig,
    VisionFineTuner,
    TrainingDataProcessor,
)
from evaluate_vision import (
    VisionEvaluator,
    EvaluationMetrics,
)


# ---------------------------------------------------------------------------
# Pipeline Configuration
# ---------------------------------------------------------------------------

@dataclass
class PipelineConfig:
    """Complete pipeline configuration."""
    # Data collection
    collect_data: bool = True
    data_output: str = "data"
    model_name: str = "llava:7b"
    
    # Fine-tuning
    fine_tune: bool = True
    num_epochs: int = 3
    learning_rate: float = 2e-5
    batch_size: int = 4
    lora_rank: int = 16
    
    # Evaluation
    evaluate: bool = True
    test_ratio: float = 0.2
    
    # Output
    output_dir: str = "models"


# ---------------------------------------------------------------------------
# Complete Pipeline
# ---------------------------------------------------------------------------

class VisionTrainingPipeline:
    """Complete vision training pipeline."""
    
    def __init__(self, config: PipelineConfig):
        self.config = config
    
    def run(self):
        """Run complete pipeline."""
        print("=" * 60)
        print("BOBBY VISION TRAINING PIPELINE")
        print("=" * 60)
        
        if self.config.collect_data:
            print("\n[1/3] Collecting training data...")
            self._collect_data()
        
        if self.config.fine_tune:
            print("\n[2/3] Fine-tuning model...")
            self._fine_tune()
        
        if self.config.evaluate:
            print("\n[3/3] Evaluating model...")
            self._evaluate()
        
        print("\n" + "=" * 60)
        print("PIPELINE COMPLETE")
        print("=" * 60)
    
    def _collect_data(self):
        """Collect training data from gauntlet runs."""
        collector = VisionDataCollector(
            output_dir=self.config.data_output,
            model_name=self.config.model_name,
        )
        
        # TODO: Integrate with Bobby runtime to collect real data
        # For now, generate synthetic data
        print("Generating synthetic training data...")
        self._generate_synthetic_data(collector)
        
        # Save dataset
        collector.save_dataset("training_data.jsonl")
        
        # Split into train/test
        self._split_dataset()
    
    def _generate_synthetic_data(self, collector: VisionDataCollector):
        """Generate synthetic training data."""
        import random
        from PIL import Image, ImageDraw
        
        journeys = [
            "customer-update",
            "onboarding",
            "documents",
            "authorization",
            "report-recovery",
        ]
        
        for journey in journeys:
            for i in range(50):  # 50 examples per journey
                success = random.random() > 0.3  # 70% success rate
                
                # Generate synthetic image
                img = Image.new("RGB", (400, 300), color="white")
                draw = ImageDraw.Draw(img)
                for _ in range(5):
                    x1, y1 = random.randint(20, 300), random.randint(20, 200)
                    x2, y2 = x1 + random.randint(50, 100), y1 + random.randint(20, 40)
                    draw.rectangle([x1, y1, x2, y2], fill=random.choice(["#3498db", "#e74c3c", "#27ae60"]))
                
                import io
                buf = io.BytesIO()
                img.save(buf, format="PNG")
                image_b64 = base64.b64encode(buf.getvalue()).decode("utf-8")
                
                collector.collect_vision_proposal(
                    screenshot_b64=image_b64,
                    purpose=random.choice([
                        "Fill login form",
                        "Navigate to dashboard",
                        "Submit contact form",
                        "Upload document",
                        "Select option from dropdown",
                    ]),
                    intent_kind=random.choice(["locate", "typeText", "extractValue"]),
                    stuck=random.choice(["targetMissing", "targetAmbiguous", "obstructionSuspected"]),
                    context={
                        "url": f"https://example.com/{journey}",
                        "candidates": [
                            {"role": "button", "name": "Submit", "ordinal": 1},
                            {"role": "textbox", "name": "Email", "ordinal": 2},
                        ],
                        "recentCommandKinds": ["navigate", "click"],
                    },
                    model_response={
                        "confidence": random.uniform(0.5, 0.95),
                        "action": {
                            "kind": "click",
                            "x": random.uniform(50, 350),
                            "y": random.uniform(50, 250),
                        },
                    },
                    success=success,
                    journey=journey,
                    step=f"step_{i}",
                    error_message="" if success else "Target element not found",
                )
    
    def _split_dataset(self):
        """Split dataset into train/test."""
        import shutil
        
        data_dir = Path(self.config.data_output)
        train_path = data_dir / "training_data.jsonl"
        test_path = data_dir / "test_data.jsonl"
        
        if not train_path.exists():
            print("No training data found. Run collection first.")
            return
        
        # Read all lines
        lines = train_path.read_text().strip().split("\n")
        
        # Shuffle
        import random
        random.shuffle(lines)
        
        # Split
        split = int(len(lines) * (1 - self.config.test_ratio))
        train_lines = lines[:split]
        test_lines = lines[split:]
        
        # Save
        train_path.write_text("\n".join(train_lines))
        test_path.write_text("\n".join(test_lines))
        
        print(f"Train: {len(train_lines)} examples")
        print(f"Test: {len(test_lines)} examples")
    
    def _fine_tune(self):
        """Fine-tune model."""
        config = FineTuneConfig(
            model_name=self.config.model_name,
            input_path=f"{self.config.data_output}/training_data.jsonl",
            output_dir=self.config.output_dir,
            learning_rate=self.config.learning_rate,
            num_epochs=self.config.num_epochs,
            batch_size=self.config.batch_size,
            lora_rank=self.config.lora_rank,
        )
        
        tuner = VisionFineTuner(config)
        tuner.fine_tune()
    
    def _evaluate(self):
        """Evaluate model."""
        config = FineTuneConfig(
            model_name=self.config.model_name,
        )
        
        evaluator = VisionEvaluator(config.model_name)
        evaluator.evaluate_dataset(f"{self.config.data_output}/test_data.jsonl")
        evaluator.print_report()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Bobby Vision Training Pipeline")
    parser.add_argument("--collect", action="store_true", help="Collect training data")
    parser.add_argument("--fine-tune", action="store_true", help="Fine-tune model")
    parser.add_argument("--evaluate", action="store_true", help="Evaluate model")
    parser.add_argument("--all", action="store_true", help="Run full pipeline")
    parser.add_argument("--journey", default=None, help="Specific journey to collect")
    parser.add_argument("--epochs", type=int, default=3, help="Number of epochs")
    parser.add_argument("--lr", type=float, default=2e-5, help="Learning rate")
    parser.add_argument("--batch-size", type=int, default=4, help="Batch size")
    parser.add_argument("--lora-rank", type=int, default=16, help="LoRA rank")
    parser.add_argument("--model", default="llava:7b", help="Base model name")
    parser.add_argument("--output", default="models", help="Output directory")
    parser.add_argument("--data", default="data", help="Data directory")
    args = parser.parse_args()
    
    # Default: run all if --all
    run_all = args.all or (not args.collect and not args.fine_tune and not args.evaluate)
    
    config = PipelineConfig(
        collect_data=args.collect or run_all,
        data_output=args.data,
        model_name=args.model,
        fine_tune=args.fine_tune or run_all,
        num_epochs=args.epochs,
        learning_rate=args.lr,
        batch_size=args.batch_size,
        lora_rank=args.lora_rank,
        evaluate=args.evaluate or run_all,
        output_dir=args.output,
    )
    
    pipeline = VisionTrainingPipeline(config)
    pipeline.run()


if __name__ == "__main__":
    main()
