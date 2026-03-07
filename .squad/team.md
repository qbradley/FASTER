# Squad Team

> Rust implementation of Microsoft FASTER — a high-performance durable hash map for mission-critical cloud services

## Coordinator

| Name | Role | Notes |
|------|------|-------|
| Squad | Coordinator | Routes work, enforces handoffs and reviewer gates. Does not generate domain artifacts. |

## Members

| Name | Role | Charter | Status |
|------|------|---------|--------|
| Gandalf | Lead / System Architect | `.squad/agents/gandalf/charter.md` | ✅ Active |
| Saruman | C++ Expert | `.squad/agents/saruman/charter.md` | ✅ Active |
| Faramir | C# Expert | `.squad/agents/faramir/charter.md` | ✅ Active |
| Aragorn | Rust Expert | `.squad/agents/aragorn/charter.md` | ✅ Active |
| Sam | Systems Programming Expert | `.squad/agents/sam/charter.md` | ✅ Active |
| Frodo | Reverse Engineer | `.squad/agents/frodo/charter.md` | ✅ Active |
| Gimli | Database/Storage Expert | `.squad/agents/gimli/charter.md` | ✅ Active |
| Galadriel | Security Expert | `.squad/agents/galadriel/charter.md` | ✅ Active |
| Legolas | Performance Guru | `.squad/agents/legolas/charter.md` | ✅ Active |
| Boromir | QA Engineer | `.squad/agents/boromir/charter.md` | ✅ Active |
| Éowyn | Det. Simulation Testing Expert | `.squad/agents/eowyn/charter.md` | ✅ Active |
| Elrond | Tokio/Async Expert | `.squad/agents/elrond/charter.md` | ✅ Active |
| Arwen | Developer Advocate | `.squad/agents/arwen/charter.md` | ✅ Active |
| Scribe | Session Logger | `.squad/agents/scribe/charter.md` | 📋 Silent |
| Ralph | Work Monitor | — | 🔄 Monitor |

## Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Description:** Production-grade Rust implementation of the FASTER durable hash map, complementing existing C++ and C# implementations. No async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI. Quality bar: largest-scale mission-critical cloud services.
- **Created:** 2026-03-05
