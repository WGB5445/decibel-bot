.PHONY: build run test clean lint

build:
	go build -o bin/decibel-bot ./cmd/decibel-bot

build-market-info:
	go build -o bin/market-info ./cmd/market-info

run: build
	./bin/decibel-bot

test:
	go test -v ./...

clean:
	rm -rf bin/

lint:
	golangci-lint run ./...

deps:
	go mod tidy
