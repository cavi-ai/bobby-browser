#!/usr/bin/env python3
"""
Bobby Vision Training Data Collector

Collects training data from gauntlet runs: screenshots, context, outcomes.
Stores as JSONL for fine-tuning vision models.

Usage:
    python collect_training_data.py --journey customer-update --output data/
    python collect_training_data.py --all --output data/
"""

import argparse
import base64
import json
import os
import sys
import time
import hashlib
from dataclasses import dataclass, field, asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Data Models
# ---------------------------------------------------------------------------

@dataclass
class VisionTrainingExample:
    """Single training example for vision model fine-tuning."""
    # Input
    image_b64: str  # Base64 encoded PNG screenshot
    purpose: str  # User's stated purpose (e.g., "Fill login form")
    intent_kind: str  # Intent type (locate, typeText, extractValue, etc.)
    stuck: str  # Stuck reason (targetMissing, targetAmbiguous, etc.)
    context_url: Optional[str] = None  # Current page URL
    context_candidates: list = field(default_factory=list)  # DOM candidates
    context_recent_commands: list = field(default_factory=list)  # Recent action types
    
    # Output (from vision model)
    model_action_kind: str = ""  # click, typeText, extractValue
    model_confidence: float = 0.0
    model_click_x: float = 0.0
    model_click_y: float = 0.0
    model_text: str = ""
    model_extracted: str = ""
    
    # Ground truth (did the action succeed?)
    success: bool = False
    journey: str = ""  # Gauntlet journey name
    step: str = ""  # Step within journey
    error_message: str = ""  # If failed, why?
    
    # Metadata
    timestamp: str = ""
    run_id: str = ""
    model_name: str = ""
    image_hash: str = ""  # SHA256 of image for dedup
    
    def to_dict(self) -> dict:
        return asdict(self)
    
    @classmethod
    def from_dict(cls, data: dict) -> "VisionTrainingExample":
        return cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})


@dataclass
class TrainingDataset:
    """Collection of training examples."""
    examples: list = field(default_factory=list)
    journey_stats: dict = field(default_factory=dict)
    
    def add(self, example: VisionTrainingExample):
        self.examples.append(example)
        journey = example.journey
        if journey not in self.journey_stats:
            self.journey_stats[journey] = {"total": 0, "success": 0, "failed": 0}
        self.journey_stats[journey]["total"] += 1
        if example.success:
            self.journey_stats[journey]["success"] += 1
        else:
            self.journey_stats[journey]["failed"] += 1
    
    def save(self, output_path: str):
        """Save dataset as JSONL."""
        Path(output_path).parent.mkdir(parents=True, exist_ok=True)
        with open(output_path, "w") as f:
            for example in self.examples:
                f.write(json.dumps(example.to_dict()) + "\n")
    
    def summary(self) -> str:
        lines = ["\n=== Training Dataset Summary ===\n"]
        lines.append(f"Total examples: {len(self.examples)}")
        for journey, stats in self.journey_stats.items():
            success_rate = stats["success"] / stats["total"] * 100 if stats["total"] > 0 else 0
            lines.append(f"  {journey}: {stats['total']} examples ({stats['success']} success, {stats['failed']} failed, {success_rate:.1f}%)")
        return "\n".join(lines)


# ---------------------------------------------------------------------------
# Data Collector
# ---------------------------------------------------------------------------

class VisionDataCollector:
    """Collects vision training data from Bobby runtime."""
    
    def __init__(self, output_dir: str, model_name: str = "llava:7b"):
        self.output_dir = Path(output_dir)
        self.output_dir.mkdir(parents=True, exist_ok=True)
        self.model_name = model_name
        self.dataset = TrainingDataset()
        self.run_id = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    
    def collect_vision_proposal(
        self,
        screenshot_b64: str,
        purpose: str,
        intent_kind: str,
        stuck: str,
        context: Optional[dict],
        model_response: Optional[dict],
        success: bool,
        journey: str,
        step: str,
        error_message: str = "",
    ) -> VisionTrainingExample:
        """Create a training example from a vision proposal."""
        
        # Compute image hash for deduplication
        image_bytes = base64.b64decode(screenshot_b64)
        image_hash = hashlib.sha256(image_bytes).hexdigest()
        
        # Extract model response
        action_kind = ""
        confidence = 0.0
        click_x = 0.0
        click_y = 0.0
        text = ""
        extracted = ""
        
        if model_response:
            action = model_response.get("action", {})
            action_kind = action.get("kind", "")
            confidence = model_response.get("confidence", 0.0)
            
            if action_kind == "click":
                click_x = action.get("x", 0.0)
                click_y = action.get("y", 0.0)
            elif action_kind == "typeText":
                text = action.get("text", "")
            elif action_kind == "extractValue":
                extracted = action.get("value", "")
        
        example = VisionTrainingExample(
            image_b64=screenshot_b64,
            purpose=purpose,
            intent_kind=intent_kind,
            stuck=stuck,
            context_url=context.get("url") if context else None,
            context_candidates=context.get("candidates", []) if context else [],
            context_recent_commands=context.get("recentCommandKinds", []) if context else [],
            model_action_kind=action_kind,
            model_confidence=confidence,
            model_click_x=click_x,
            model_click_y=click_y,
            model_text=text,
            model_extracted=extracted,
            success=success,
            journey=journey,
            step=step,
            error_message=error_message,
            timestamp=datetime.now(timezone.utc).isoformat(),
            run_id=self.run_id,
            model_name=self.model_name,
            image_hash=image_hash,
        )
        
        self.dataset.add(example)
        return example
    
    def save_dataset(self, filename: str = "training_data.jsonl"):
        """Save collected dataset to file."""
        output_path = self.output_dir / filename
        self.dataset.save(str(output_path))
        print(self.dataset.summary())
        print(f"\nDataset saved to: {output_path}")
        return output_path
    
    def collect_from_gauntlet_run(
        self,
        journey: str,
        steps: list,
    ) -> TrainingDataset:
        """
        Collect training data from a single gauntlet journey run.
        
        Args:
            journey: Journey name (e.g., "customer-update")
            steps: List of step dicts with keys:
                - screenshot_b64: PNG screenshot
                - purpose: User's purpose
                - intent_kind: Intent type
                - stuck: Stuck reason
                - context: Optional context dict
                - model_response: Optional model response dict
                - success: Whether step succeeded
                - error_message: Optional error message
        """
        for i, step in enumerate(steps):
            self.collect_vision_proposal(
                screenshot_b64=step["screenshot_b64"],
                purpose=step.get("purpose", ""),
                intent_kind=step.get("intent_kind", "locate"),
                stuck=step.get("stuck", "targetMissing"),
                context=step.get("context"),
                model_response=step.get("model_response"),
                success=step.get("success", False),
                journey=journey,
                step=f"step_{i}",
                error_message=step.get("error_message", ""),
            )
        
        return self.dataset


# ---------------------------------------------------------------------------
# Integration with Bobby Runtime
# ---------------------------------------------------------------------------

def integrate_with_bobby_runtime():
    """
    Hook into Bobby's vision proxy to automatically collect training data.
    
    This function patches the vision proxy to log all proposals to disk.
    """
    import requests
    from http.server import HTTPServer, BaseHTTPRequestHandler
    
    # Create a logging proxy that wraps the real vision provider
    class LoggingVisionHandler(BaseHTTPRequestHandler):
        collector: Optional[VisionDataCollector] = None
        
        def do_POST(self):
            if self.path != "/propose":
                self.send_response(404)
                self.end_headers()
                return
            
            content_length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_length)
            
            try:
                request = json.loads(body)
            except:
                self.send_response(400)
                self.end_headers()
                return
            
            # Log the proposal
            if self.collector:
                try:
                    self.collector.collect_vision_proposal(
                        screenshot_b64=request.get("screenshotPng", ""),
                        purpose=request.get("purpose", ""),
                        intent_kind=request.get("intentKind", "locate"),
                        stuck=request.get("stuck", "targetMissing"),
                        context=request.get("context"),
                        model_response=None,  # Will be set when response comes back
                        success=False,  # Unknown until gauntlet completes
                        journey="unknown",
                        step="unknown",
                    )
                except Exception as e:
                    print(f"Error logging proposal: {e}")
            
            # Forward to real provider
            # (In production, this would call the real Ollama/MLX provider)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
    
    return LoggingVisionHandler


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Bobby Vision Training Data Collector")
    parser.add_argument("--output", default="data", help="Output directory")
    parser.add_argument("--journey", default=None, help="Specific journey to collect")
    parser.add_argument("--all", action="store_true", help="Collect from all journeys")
    parser.add_argument("--model", default="llava:7b", help="Model name")
    parser.add_argument("--simulate", action="store_true", help="Generate synthetic training data")
    args = parser.parse_args()
    
    collector = VisionDataCollector(args.output, args.model)
    
    if args.simulate:
        print("Generating synthetic training data...")
        generate_synthetic_data(collector)
    else:
        print("Waiting for gauntlet runs...")
        print("Use --simulate to generate synthetic data for testing.")
    
    collector.save_dataset()


def generate_synthetic_data(collector: VisionDataCollector):
    """Generate synthetic training data for testing."""
    import random
    
    journeys = [
        "customer-update",
        "onboarding",
        "documents",
        "authorization",
        "report-recovery",
    ]
    
    purposes = [
        "Fill login form",
        "Navigate to dashboard",
        "Submit contact form",
        "Upload document",
        "Select option from dropdown",
        "Click navigation link",
        "Extract page value",
    ]
    
    stuck_types = ["targetMissing", "targetAmbiguous", "obstructionSuspected"]
    
    for journey in journeys:
        for i in range(20):  # 20 examples per journey
            success = random.random() > 0.3  # 70% success rate
            
            collector.collect_vision_proposal(
                screenshot_b64=generate_synthetic_image(),
                purpose=random.choice(purposes),
                intent_kind=random.choice(["locate", "typeText", "extractValue"]),
                stuck=random.choice(stuck_types),
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
    
    collector.save_dataset()


def generate_synthetic_image() -> str:
    """Generate a synthetic PNG image for testing."""
    from PIL import Image, ImageDraw
    
    img = Image.new("RGB", (400, 300), color="white")
    draw = ImageDraw.Draw(img)
    
    # Draw some random UI elements
    for _ in range(5):
        x1, y1 = random.randint(20, 300), random.randint(20, 200)
        x2, y2 = x1 + random.randint(50, 100), y1 + random.randint(20, 40)
        draw.rectangle([x1, y1, x2, y2], fill=random.choice(["#3498db", "#e74c3c", "#27ae60"]))
    
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return base64.b64encode(buf.getvalue()).decode("utf-8")


if __name__ == "__main__":
    import io
    import random
    main()
