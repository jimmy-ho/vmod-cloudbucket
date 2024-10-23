AWS_PROFILE :=
AWS_BUCKET_NAME := 
AWS_BUCKET_REGION := 

.PHONY: build
build:
	docker build --progress=plain . -t vmod-cloudbucket-varnish:0.0.1

.PHONY: run
run:
	docker run \
		-ti \
		-d \
		-e AWS_PROFILE=$(AWS_PROFILE) \
		-e AWS_BUCKET_NAME=$(AWS_BUCKET_NAME) \
		-e AWS_BUCKET_REGION=$(AWS_BUCKET_REGION) \
		-v ~/.aws:/nonexistent/.aws:ro \
		-v ./example/default.vcl:/etc/varnish/default.vcl:ro \
		--tmpfs /var/lib/varnish/varnishd:exec \
		-p 8787:80 \
		vmod-cloudbucket-varnish:0.0.1 varnishd -F -f /etc/varnish/default.vcl -s malloc,256m -p vsl_reclen=4084

.PHONY: lint
lint:
	cargo fix --allow-staged --allow-dirty && cargo fmt
