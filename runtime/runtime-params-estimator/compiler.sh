#!/bin/bash

./setup.sh

VMKIND="wasmer"
features=""
if [ "$1" == "wasmtime" ]; then
  VMKIND="$1";
fi
if [ "$1" == "lightbeam" ]; then
  VMKIND="wasmtime";
  features="--features lightbeam"
fi


set -ex

cargo build --release --package runtime-params-estimator $features
./emu-cost/counter_plugin/qemu-x86_64 -cpu Westmere-v1 -plugin file=./emu-cost/counter_plugin/libcounter.so ../../target/release/runtime-params-estimator --compile-only --vm-kind "$VMKIND"
