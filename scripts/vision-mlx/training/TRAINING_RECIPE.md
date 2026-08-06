# Bobby Vision Model Training Recipe

## Overview

This recipe defines the complete training pipeline for Bobby's vision model:
data collection, fine-tuning, and evaluation.

## Goals

- **Primary**: Improve vision model accuracy on Bobby's specific web automation tasks
- **Secondary**: Reduce reliance on cloud APIs (OpenAI)
- **Tertiary**: Enable offline/local vision assistance

## Current State

- **Working**: Ollama llava:7b with Bobby schema normalization
- **Accuracy**: ~75% action prediction, ~40% clicks within 10px
- **Speed**: ~2s per vision call on M5 Max

## Training Pipeline

### 1. Data Collection

**Source**: Bobby's gauntlet runs (5 journeys)
- `customer-update`: Customer search and priority update
- `onboarding`: User registration form
- `documents`: Document upload with iframe confirmation
- `authorization`: Popup authorization with obstruction
- `report-recovery`: Interrupted report recovery with download

**Data per example**:
```json
{
  "image_b64": "<base64 PNG screenshot>",
  "purpose": "Fill login form",
  "intent_kind": "locate",
  "stuck": "targetMissing",
  "context": {
    "url": "https://example.com/login",
    "candidates": [{"role": "button", "name": "Login"}],
    "recentCommandKinds": ["navigate"]
  },
  "model_response": {
    "confidence": 0.85,
    "action": {"kind": "click", "x": 150.0, "y": 200.0}
  },
  "success": true,
  "journey": "onboarding",
  "step": "step_3"
}
```

**Collection methods**:
1. **Runtime hook**: Patch Bobby's vision proxy to log all proposals
2. **Gauntlet integration**: Capture data from each journey step
3. **Synthetic data**: Generate synthetic UI screenshots for testing

**Target dataset size**: 5,000+ examples (1,000 per journey)

### 2. Fine-Tuning

**Base model**: Qwen2-VL-7B (when MLX vision support lands) or llava:7b (Ollama)

**Fine-tuning approach**: LoRA (Low-Rank Adaptation)
- LoRA rank: 16
- LoRA alpha: 32.0
- Learning rate: 2e-5
- Epochs: 3
- Batch size: 4

**Training steps**:
1. Preprocess images (resize, normalize)
2. Build prompts from context + purpose
3. Create targets from ground-truth actions
4. Train with LoRA adapters
5. Save fine-tuned model

**Compute requirements**:
- M5 Max with 128GB RAM
- ~14GB VRAM for 7B model (quantized)
- ~2 hours training time

### 3. Evaluation

**Metrics**:
- **Action accuracy**: Did model predict correct action type? (click/typeText/extractValue)
- **Coordinate MAE**: Mean absolute error for click coordinates (pixels)
- **Coordinate precision**: Fraction of clicks within 10px/50px of target
- **Confidence calibration**: Does model confidence match actual accuracy?
- **Journey success rate**: Fraction of journeys completed successfully

**Evaluation harness**:
1. Run all 5 gauntlet journeys
2. Capture vision proposals and outcomes
3. Compute metrics per journey and overall
4. Compare before/after fine-tuning

**Target improvements**:
- Action accuracy: 75% → 90%
- Coordinate MAE: 25px → 10px
- Journey success rate: 70% → 85%

## Integration with Bobby

### Runtime Data Collection

Patch Bobby's vision proxy to automatically collect training data:

```python
# In vision-proxy/src/ollama.rs
async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError> {
    // ... existing code ...
    
    // Log to training data store
    log_training_example(ProposeInput {
        screenshot_png_b64: input.screenshot_png_b64,
        purpose: input.purpose.clone(),
        intent_kind: input.intent_kind.clone(),
        stuck: input.stuck.clone(),
        context: input.context.clone(),
    });
    
    // ... existing code ...
}
```

### Gauntlet Integration

Modify gauntlet tests to capture vision data:

```rust
// In modern_gauntlet_e2e.rs
#[tokio::test]
async fn customer_discovery_and_update_is_durable() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("customer-update")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
    
    // Capture vision data for each step
    runtime.type_text("input[aria-label='Search customers']", "Atlas").await?;
    capture_vision_data(&runtime, "customer-update", "step_search").await?;
    
    runtime.click("form[aria-label='Customer search'] button", false).await?;
    capture_vision_data(&runtime, "customer-update", "step_click").await?;
    
    // ... rest of test ...
}
```

## Model Selection

### Current (Ollama)
- **Model**: llava:7b
- **Speed**: ~2s per call
- **Accuracy**: ~75% action, ~40% clicks within 10px
- **RAM**: ~5GB
- **Status**: Working

### Future (MLX)
- **Model**: Qwen2-VL-7B (when MLX adds vision support)
- **Speed**: ~1s per call (expected)
- **Accuracy**: ~90% action, ~60% clicks within 10px (expected)
- **RAM**: ~14GB
- **Status**: Waiting for MLX vision support

### Cloud Fallback
- **Model**: gpt-4o (OpenAI)
- **Speed**: ~0.5s per call
- **Accuracy**: ~95% action, ~80% clicks within 10px
- **Cost**: ~$0.01-0.05 per call
- **Status**: Available

## Training Schedule

### Phase 1: Data Collection (Week 1-2)
- Collect 1,000 examples per journey (5,000 total)
- Focus on failure cases (low confidence, wrong actions)
- Store as JSONL with images

### Phase 2: Baseline Evaluation (Week 2)
- Evaluate current model on test set
- Establish baseline metrics
- Identify failure modes

### Phase 3: Fine-Tuning (Week 3)
- Fine-tune Qwen2-VL-7B with LoRA
- Experiment with different hyperparameters
- Save best checkpoint

### Phase 4: Evaluation (Week 4)
- Evaluate fine-tuned model
- Compare to baseline
- Document improvements

### Phase 5: Iteration (Ongoing)
- Collect more data from production
- Retrain periodically
- Monitor for drift

## Files

- `scripts/vision-mlx/collect_training_data.py` - Data collection
- `scripts/vision-mlx/training/fine_tune_vision.py` - Fine-tuning
- `scripts/vision-mlx/training/evaluate_vision.py` - Evaluation
- `scripts/vision-mlx/training/run_training_pipeline.py` - Complete pipeline

## Next Steps

1. **Integrate data collection** into Bobby's runtime (patch vision proxy)
2. **Run gauntlet** to collect real training data
3. **Fine-tune** Qwen2-VL-7B when MLX adds vision support
4. **Evaluate** improvements on gauntlet journeys
