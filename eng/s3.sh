#!/bin/sh
# The local S3 test store used by docs/storage.md and the CI gate.
set -eu

name=neha-s3
image=chrislusf/seaweedfs@sha256:7ca46aeae2f223b37e5d91593c17ab37738eec6df6ab69b48a0487f7643342b9
endpoint=http://127.0.0.1:8333
bucket=neha

case ${1:-} in
up)
  docker rm -f $name >/dev/null 2>&1 || true
  docker run -d --name $name -p 8333:8333 $image server -s3 -dir=/data >/dev/null
  attempt=0
  until curl -fsS --max-time 2 $endpoint/ >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ $attempt -ge 30 ]; then
      echo "the S3 test store did not start" >&2
      docker logs $name >&2
      exit 1
    fi
    sleep 1
  done
  curl -fsS --max-time 5 -X PUT $endpoint/$bucket
  echo "the S3 test store is ready at $endpoint/$bucket"
  ;;
down)
  docker rm -f $name >/dev/null
  ;;
logs)
  docker logs $name
  ;;
*)
  echo "usage: eng/s3.sh up|down|logs" >&2
  exit 2
  ;;
esac
