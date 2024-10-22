# vmod-cloudbucket

This is a vmod for [varnish](http://varnish-cache.org/) to send and receive HTTP requests from VCL for cloud buckets with automatic authorization and acting as a backend, leveraging the [reqwest crate](https://docs.rs/reqwest/latest/reqwest/).

This vmod is based on [vmod-reqwest](https://github.com/gquintard/vmod-reqwest).

Currently this only supports AWS S3 for now.

Refer to [example](https://github.com/jimmy-ho/vmod-cloudbucket/tree/cloudbucket/example) for a `default.vcl` on its use.