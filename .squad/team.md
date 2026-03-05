# Squad Team

> Rust implementation of Microsoft FASTER — a high-performance durable hash map for mission-critical cloud services

## Coordinator

| Name | Role | Notes |
|------|------|-------|
| Squad | Coordinator | Routes work, enforces handoffs and reviewer gates. Does not generate domain artifacts. |

## Members

| Name | Role | Charter | Status |
|------|------|---------|--------|
| Thrawn | Lead / System Architect | `.squad/agents/thrawn/charter.md` | ✅ Active |
| Grievous | C++ Expert | `.squad/agents/grievous/charter.md` | ✅ Active |
| Dooku | C# Expert | `.squad/agents/dooku/charter.md` | ✅ Active |
| Mando | Rust Expert | `.squad/agents/mando/charter.md` | ✅ Active |
| Chirrut | Systems Programming Expert | `.squad/agents/chirrut/charter.md` | ✅ Active |
| Cassian | Reverse Engineer | `.squad/agents/cassian/charter.md` | ✅ Active |
| Tarkin | Database/Storage Expert | `.squad/agents/tarkin/charter.md` | ✅ Active |
| Maul | Security Expert | `.squad/agents/maul/charter.md` | ✅ Active |
| Ahsoka | Performance Guru | `.squad/agents/ahsoka/charter.md` | ✅ Active |
| Rex | QA Engineer | `.squad/agents/rex/charter.md` | ✅ Active |
| Jyn | Det. Simulation Testing Expert | `.squad/agents/jyn/charter.md` | ✅ Active |
| Kenobi | Tokio/Async Expert | `.squad/agents/kenobi/charter.md` | ✅ Active |
| Scribe | Session Logger | `.squad/agents/scribe/charter.md` | 📋 Silent |
| Ralph | Work Monitor | — | 🔄 Monitor |

## Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Description:** Production-grade Rust implementation of the FASTER durable hash map, complementing existing C++ and C# implementations. No async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI. Quality bar: largest-scale mission-critical cloud services.
- **Created:** 2026-03-05
