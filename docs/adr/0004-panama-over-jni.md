# 0004. The JVM binds through Panama, with JNI as a fallback provider

Status: accepted, 2026-08-15. Spec: `Spec/2064g/dx/08-jvm.md`.

## Context

There are two ways for the JVM to reach `libzu`. JNI, which has been there since Java 1.1 and needs a hand-written C shim compiled per platform. Or the Foreign Function and Memory API, finalised in JDK 22, where `jextract` generates the bindings from `zu.h` and there is no C shim at all.

FFM is better on every axis that matters here. No native code beyond `libzu` itself, so no per-platform shim to build, sign, and ship. `MemorySegment` over the result arena is genuinely zero-copy, where JNI reaches the same data through `ByteBuffer` with more copying and the `GetPrimitiveArrayCritical` hazards. Memory lifetime is an `Arena`, which is deterministic and bounded rather than manual.

The problem is the floor. FFM needs Java 22, and a large part of the enterprise ecosystem is on 17 or 21. Requiring 22 in 2026 excludes users we have no reason to exclude.

## Decision

FFM is the primary path. JNI stays as a fallback provider for Java 17 to 21, and the two are separate artifacts behind one API:

| Artifact | Baseline | Role |
|---|---|---|
| `dev.zudb:zudb` | Java 17 | the API. No native code, no FFM types in any signature |
| `dev.zudb:zudb-ffm` | Java 22+ | the FFM provider |
| `dev.zudb:zudb-jni` | Java 17+ | the fallback provider |
| `dev.zudb:zudb-native-{platform}` | | the `libzu` binaries |

A `ServiceLoader` picks the provider at runtime and logs the choice once at debug level. Application code never names one. The baseline for the modern artifact is Java 25 LTS, and CI runs 17, 21, 25, and 26.

The API artifact must not leak `MemorySegment` into any public signature. That constraint is what keeps the Java 17 artifact honest, and it is checked rather than remembered.

## Consequences

Users on a current JDK get zero-copy column access and an install with no shim. Users on 17 get a working client. Neither has to know which provider they are on.

Two providers means two code paths for the same semantics, which is real cost, and the answer is that both run the same conformance corpus on every supported JDK. A behavioural difference between them is a bug the corpus catches rather than a thing a user reports.

From JDK 24, native access must be granted explicitly or the JVM warns, and the stated direction is that it becomes an error. This affects both providers, not just FFM, so both carry `Enable-Native-Access: ALL-UNNAMED` in the manifest for the classpath case, the docs give `--enable-native-access=dev.zudb` for the module path, and the binding detects the ungranted state at `Database.open` and throws a message containing the flag to add. A JVM warning printed to stderr three frames from any of our code is not something a user can act on.

## Rejected

**JNI only.** A hand-written C shim per platform, more copying on the hot path, and manual memory lifetime, in exchange for supporting a JDK line that is already the minority and shrinking.

**FFM only, Java 22 floor.** Cleaner to build and it abandons Java 17 and 21 users for no engineering reason other than our convenience.

**Java 22 as the modern baseline.** FFM was finalised there, which is the only argument for it, and JDK 22 has been end of life since September 2024. Building a 2026 baseline on an unsupported release buys nothing. Java 25 LTS is supported to 2030.
