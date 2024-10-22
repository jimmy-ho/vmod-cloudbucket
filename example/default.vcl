vcl 4.1;

import dynamic;
import std;
import cloudbucket;

backend default none;

sub vcl_init {
  new be = cloudbucket.client(s3_bucket = std.getenv("AWS_BUCKET_NAME"), region = std.getenv("AWS_BUCKET_REGION"));
}

sub vcl_recv {
  unset req.http.cookie;

  set req.backend_hint = be.backend();

  if (req.method != "GET" && req.method != "HEAD") {
      return (pass);
  }

  return (hash);
}

sub vcl_synth {
}

sub vcl_backend_response {
  # Stream the backend response.
  set beresp.do_stream = true;
  set beresp.ttl = 60s;
}