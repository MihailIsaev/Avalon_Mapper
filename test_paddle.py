from paddleocr import PaddleOCR
import sys

ocr = PaddleOCR(
    lang="en",
    use_textline_orientation=True,
)

result = ocr.predict(sys.argv[1])

for page in result:
    # новый PaddleOCR возвращает объект/словарь с rec_texts и rec_scores
    texts = page.get("rec_texts", [])
    scores = page.get("rec_scores", [])

    for text, score in zip(texts, scores):
        print(f"{score:.3f}: {text}")