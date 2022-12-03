build-alpine:
	rustup target add x86_64-unknown-linux-musl
	cargo build --release --target=x86_64-unknown-linux-musl

upload:
	./upload.sh

login-gcloud-user:
	gcloud auth login
	gcloud config set project devbookhq
