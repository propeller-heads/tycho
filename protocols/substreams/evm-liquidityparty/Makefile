all: ethereum

build:
	cargo build --target wasm32-unknown-unknown --release

ethereum: build
	substreams pack ethereum-liquidityparty.yaml -o ethereum-liquidityparty.spkg

.PHONY: build ethereum
