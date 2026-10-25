# fleet/tools: operator scripts. `make test` runs their tests (no GPU, no coordinator).
.PHONY: test
test:
	PYTHONDONTWRITEBYTECODE=1 python3 -m unittest -v test_tools
