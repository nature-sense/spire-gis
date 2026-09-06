# spire-gis — Rust core (crates/spire-gis) + SwiftUI app (ui/swift).
#
#   make rust   — build the Rust core (libspire-gis.dylib)
#   make swift  — build the SwiftUI executable
#   make app    — build everything + assemble build/spire-gis.app
#   make run    — assemble + launch the app
#   make clean  — remove build artifacts

.PHONY: rust swift app run clean

rust:
	cargo build --release -p spire-gis

swift:
	cd ui/swift && swift build

app:
	@./build/assemble-app.sh

run: app
	@open ./build/spire-gis.app

clean:
	cargo clean
	rm -rf build/spire-gis.app
	cd ui/swift && swift package clean || true
