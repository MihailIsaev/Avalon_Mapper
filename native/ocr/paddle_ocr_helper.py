#!/usr/bin/env python3
import json
import faulthandler
import os
import re
import sys
import time

os.environ.setdefault("PADDLE_PDX_MODEL_SOURCE", "BOS")
faulthandler.enable(file=sys.stderr, all_threads=True)

MODEL_NAME = "en_PP-OCRv5_mobile_rec"
DETECTION_MODEL_NAME = "PP-OCRv5_mobile_det"
ocr = None
engine_name = "ocr:auto"

def create_rapidocr():
    from rapidocr_onnxruntime import RapidOCR

    return RapidOCR()

def create_paddleocr(kwargs):
    from paddleocr import PaddleOCR

    return PaddleOCR(**kwargs)

def initialize_ocr():
    global engine_name

    source = os.environ.get("PADDLE_PDX_MODEL_SOURCE")
    rapidocr_attempt = (
        "rapidocr:onnxruntime",
        lambda: create_rapidocr(),
        "onnxruntime",
    )
    default_paddle_attempt = (
        "paddleocr:en_default",
        lambda: create_paddleocr({
            "lang": "en",
            "use_doc_orientation_classify": False,
            "use_doc_unwarping": False,
            "use_textline_orientation": False,
            "text_rec_score_thresh": 0.45,
        }),
        "detection_model=default recognition_model=default",
    )
    named_paddle_attempt = (
        f"paddleocr:{MODEL_NAME}",
        lambda: create_paddleocr({
            "lang": "en",
            "text_detection_model_name": DETECTION_MODEL_NAME,
            "text_recognition_model_name": MODEL_NAME,
            "use_doc_orientation_classify": False,
            "use_doc_unwarping": False,
            "use_textline_orientation": False,
            "text_rec_score_thresh": 0.45,
        }),
        f"detection_model={DETECTION_MODEL_NAME} recognition_model={MODEL_NAME}",
    )
    if sys.platform == "win32":
        print(
            "[ocr] windows detected; using RapidOCR ONNXRuntime",
            file=sys.stderr,
            flush=True,
        )
        attempts = [rapidocr_attempt]
    else:
        attempts = [named_paddle_attempt, default_paddle_attempt]

    errors = []
    for name, factory, model_summary in attempts:
        print(
            f"[ocr] initializing engine={name} {model_summary} source={source}",
            file=sys.stderr,
            flush=True,
        )
        try:
            instance = factory()
            engine_name = name
            print(f"[ocr] initialized engine={engine_name}", file=sys.stderr, flush=True)
            return instance
        except BaseException as exc:
            errors.append(f"{name}: {type(exc).__name__}: {exc}")
            print(
                f"[ocr] init_attempt_failed engine={name} error={type(exc).__name__}: {exc}",
                file=sys.stderr,
                flush=True,
            )

    raise RuntimeError("Could not initialize OCR: " + " | ".join(errors))

def get_ocr():
    global ocr
    if ocr is None:
        ocr = initialize_ocr()
    return ocr

def is_bad_current_location_line(text: str) -> bool:
    text = text.strip()

    if not text:
        return True

    # 0-5 VI / 0 - 5 VI
    if re.fullmatch(r"\d+\s*-\s*\d+\s*[IVX]+", text, re.IGNORECASE):
        return True

    # VI / IV / VII
    if re.fullmatch(r"[IVX]+", text, re.IGNORECASE):
        return True

    # 02:2 / 02:29 / 1:05
    if re.fullmatch(r"\d{1,2}:\d{1,2}", text):
        return True

    # only numbers/symbols
    letters = sum(ch.isalpha() for ch in text)
    if letters < 3:
        return True

    return False

def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--server":
        try:
            get_ocr()
        except BaseException as exc:
            print(
                f"[ocr] init_failed error={type(exc).__name__}: {exc}",
                file=sys.stderr,
                flush=True,
            )
            sys.exit(2)

        print(f"[ocr] server_ready engine={engine_name}", file=sys.stderr, flush=True)
        for line in sys.stdin:
            try:
                request = json.loads(line)
                kind = request.get("kind", "current")
                image_path = request["image_path"]
                print(json.dumps(run_ocr(kind, image_path), ensure_ascii=False), flush=True)
            except Exception as exc:
                print(json.dumps({
                    "ok": False,
                    "error": str(exc),
                }), flush=True)
        return

    if len(sys.argv) < 3:
        print(json.dumps({
            "ok": False,
            "error": "Usage: paddle_ocr_helper.py <kind> <image_path>"
        }))
        sys.exit(1)

    kind = sys.argv[1]
    image_path = sys.argv[2]
    print(json.dumps(run_ocr(kind, image_path), ensure_ascii=False))

def run_ocr(kind: str, image_path: str) -> dict:
    started = time.perf_counter()
    lines = run_engine(image_path)
    duration_ms = int((time.perf_counter() - started) * 1000)

    if kind == "current":
        parsed_text, parsed_confidence = select_current_location_text(lines)
    else:
        parsed_text = "\n".join(line["text"] for line in lines if line["text"])
        parsed_confidence = max(
            [line["confidence"] or 0.0 for line in lines],
            default=0.0
        )

    print(
        f"[ocr] engine={engine_name} kind={kind} image={image_path} "
        f"duration_ms={duration_ms} raw_lines={lines}",
        file=sys.stderr,
        flush=True,
    )

    return {
        "ok": True,
        "engine": engine_name,
        "text": parsed_text,
        "confidence": parsed_confidence,
        "lines": lines,
        "duration_ms": duration_ms,
    }

def select_current_location_text(lines: list[dict]) -> tuple[str, float | None]:
    candidates = [
        line for line in sorted_current_location_lines(lines)
        if not is_bad_current_location_line(line["text"])
        and (line["confidence"] is None or line["confidence"] >= 0.45)
    ]

    if not candidates:
        return "", None

    text = " ".join(line["text"].strip() for line in candidates if line["text"].strip())
    scores = [line["confidence"] for line in candidates if line["confidence"] is not None]
    confidence = min(scores) if scores else None
    return text, confidence

def sorted_current_location_lines(lines: list[dict]) -> list[dict]:
    return sorted(
        lines,
        key=lambda line: (
            line.get("bbox", {}).get("y", 0.0),
            line.get("bbox", {}).get("x", 0.0),
        ),
    )

def run_engine(image_path: str) -> list[dict]:
    engine = get_ocr()
    if engine_name.startswith("rapidocr:"):
        result, _ = engine(image_path)
        if not result:
            return []
        lines = []
        for item in result:
            if len(item) < 3:
                continue
            box, text, score = item[0], item[1], item[2]
            text = (text or "").strip()
            if not text:
                continue
            line = {
                "text": text,
                "confidence": float(score) if score is not None else None
            }
            bbox = bbox_from_rapidocr_box(box)
            if bbox is not None:
                line["bbox"] = bbox
            lines.append(line)
        return lines

    result = engine.predict(image_path)
    lines = []

    for page in result:
        texts = page.get("rec_texts", [])
        scores = page.get("rec_scores", [])

        for text, score in zip(texts, scores):
            text = (text or "").strip()
            if not text:
                continue
            lines.append({
                "text": text,
                "confidence": float(score) if score is not None else None
            })

    return lines

def bbox_from_rapidocr_box(box):
    try:
        xs = [float(point[0]) for point in box]
        ys = [float(point[1]) for point in box]
        left = min(xs)
        top = min(ys)
        return {
            "x": left,
            "y": top,
            "width": max(xs) - left,
            "height": max(ys) - top,
        }
    except Exception:
        return None

if __name__ == "__main__":
    main()
