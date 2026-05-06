#!/usr/bin/env python3
import json
import faulthandler
import os
import re
import sys
import time

os.environ.setdefault("PADDLE_PDX_MODEL_SOURCE", "BOS")
faulthandler.enable(file=sys.stderr, all_threads=True)

from paddleocr import PaddleOCR

MODEL_NAME = "en_PP-OCRv5_mobile_rec"
DETECTION_MODEL_NAME = "PP-OCRv5_mobile_det"
ocr = None
engine_name = f"paddleocr:{MODEL_NAME}"

def initialize_ocr():
    global engine_name

    source = os.environ.get("PADDLE_PDX_MODEL_SOURCE")
    attempts = [
        (
            f"paddleocr:{MODEL_NAME}",
            {
                "lang": "en",
                "text_detection_model_name": DETECTION_MODEL_NAME,
                "text_recognition_model_name": MODEL_NAME,
                "use_doc_orientation_classify": False,
                "use_doc_unwarping": False,
                "use_textline_orientation": False,
                "text_rec_score_thresh": 0.45,
            },
        ),
        (
            "paddleocr:en_default",
            {
                "lang": "en",
                "use_doc_orientation_classify": False,
                "use_doc_unwarping": False,
                "use_textline_orientation": False,
                "text_rec_score_thresh": 0.45,
            },
        ),
    ]

    errors = []
    for name, kwargs in attempts:
        model_summary = (
            f"detection_model={kwargs.get('text_detection_model_name', 'default')} "
            f"recognition_model={kwargs.get('text_recognition_model_name', 'default')}"
        )
        print(
            f"[paddleocr] initializing {model_summary} source={source}",
            file=sys.stderr,
            flush=True,
        )
        try:
            instance = PaddleOCR(**kwargs)
            engine_name = name
            print(f"[paddleocr] initialized engine={engine_name}", file=sys.stderr, flush=True)
            return instance
        except BaseException as exc:
            errors.append(f"{name}: {type(exc).__name__}: {exc}")
            print(
                f"[paddleocr] init_attempt_failed engine={name} error={type(exc).__name__}: {exc}",
                file=sys.stderr,
                flush=True,
            )

    raise RuntimeError("Could not initialize PaddleOCR: " + " | ".join(errors))

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
                f"[paddleocr] init_failed error={type(exc).__name__}: {exc}",
                file=sys.stderr,
                flush=True,
            )
            sys.exit(2)

        print(f"[paddleocr] server_ready engine={engine_name}", file=sys.stderr, flush=True)
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
    result = get_ocr().predict(image_path)
    duration_ms = int((time.perf_counter() - started) * 1000)

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

    if kind == "current":
        candidates = [
            line for line in lines
            if not is_bad_current_location_line(line["text"])
            and (line["confidence"] is None or line["confidence"] >= 0.45)
        ]

        if candidates:
            best = max(candidates, key=lambda x: x["confidence"] or 0.0)
            parsed_text = best["text"]
            parsed_confidence = best["confidence"]
        else:
            parsed_text = ""
            parsed_confidence = None
    else:
        parsed_text = "\n".join(line["text"] for line in lines if line["text"])
        parsed_confidence = max(
            [line["confidence"] or 0.0 for line in lines],
            default=0.0
        )

    print(
        f"[paddleocr] model={MODEL_NAME} kind={kind} image={image_path} "
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

if __name__ == "__main__":
    main()
