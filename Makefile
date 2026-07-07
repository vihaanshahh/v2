.PHONY: check test build release package

check: test build

test:
	cargo test

build:
	cargo build --release

release: build

package: build
	@mkdir -p dist
	@target=$$(rustc -vV | sed -n 's/^host: //p'); \
	case "$$target" in \
	  x86_64-apple-darwin) artifact=v2-darwin-x64 ;; \
	  aarch64-apple-darwin) artifact=v2-darwin-arm64 ;; \
	  x86_64-unknown-linux-gnu) artifact=v2-linux-x64 ;; \
	  aarch64-unknown-linux-gnu) artifact=v2-linux-arm64 ;; \
	  *) echo "unsupported host target: $$target"; exit 1 ;; \
	esac; \
	mkdir -p .pkg && cp target/release/v2 .pkg/v2 && chmod +x .pkg/v2; \
	tar czf "dist/$$artifact.tar.gz" -C .pkg v2; \
	rm -rf .pkg; \
	echo "wrote dist/$$artifact.tar.gz"
