.PHONY: route calibrate test

# Score a prompt and print a model recommendation, e.g.
#   make route PROMPT=path/to/prompt.md
route:
	cargo run -q --bin wayfinder-router -- route $(PROMPT)

# Calibrate a routing config from a labeled JSONL dataset, e.g.
#   make calibrate DATA=data.jsonl MODE=threshold
calibrate:
	cargo run -q --bin wayfinder-router -- calibrate $(DATA) --mode $(MODE)

test:
	cargo test
