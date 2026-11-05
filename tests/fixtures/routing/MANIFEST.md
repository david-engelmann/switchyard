# Routing fixtures

This directory holds the recorded fixtures of the routing slice. Each fixture file carries one case in full, the description exactly as written, the exact request document the library built for it, the exact reply the case settled on, the pool geometry of the replay stack, the declared default profile, the routing log line the case produced, and the claim outcome the reply settles.

The 48 live cases were recorded through the finished binary in one recording window, one call per case, with the credentialed wrapper standing behind the judge command seam. Each recorded routing log line was cross-checked field by field against an independent mirror of the reply grammar before its fixture was written, and no reply has been edited since. The request each fixture carries is the request document itself. The judge command line protocol adds one framing newline on the wire, and that newline is not part of the document and is not recorded.

The seven constructed cases were built by hand to stand in the failure classes of the reply grammar that a live window cannot be ordered to produce. Their request documents are the exact documents the library builds for their descriptions, generated from the library itself, and their expected lines were computed with the same independent mirror.

Provenance values are window for the 36 descriptions of the Anthropic window corpus, new for the twelve doctrine seam cases the routing design adds, and constructed for the seven hand built failure cases. The doctrine truth column states the band the settled doctrine assigns the description, and the recorded verdict column states what the model in fact answered, and the two disagree wherever the model reached for the none of these label or another band. The replay pins in tests/routing.rs hold the fixtures to their recorded outcomes, never to the doctrine truth.

| Case | Provenance | Family | Doctrine truth | Recorded verdict | Fallback | Allocated |
| --- | --- | --- | --- | --- | --- | --- |
| SW-F01 | constructed | constructed failure | none | none | silence | standard |
| SW-F02 | constructed | constructed failure | none | none | silence | lean |
| SW-F03 | constructed | constructed failure | none | none | malformed | standard |
| SW-F04 | constructed | constructed failure | none | none | malformed | lean |
| SW-F05 | constructed | constructed failure | none | other | abstention | standard |
| SW-F06 | constructed | constructed failure | none | standard | low confidence | lean |
| SW-F07 | constructed | constructed failure | none | none | malformed | standard |
| SW-L01 | window | window corpus | lean | other | abstention | standard |
| SW-L02 | window | window corpus | lean | other | abstention | standard |
| SW-L03 | window | window corpus | lean | lean | none | lean |
| SW-L04 | window | window corpus | lean | other | abstention | standard |
| SW-L05 | window | window corpus | lean | lean | none | lean |
| SW-L06 | window | window corpus | lean | other | abstention | standard |
| SW-L07 | window | window corpus | lean | lean | none | lean |
| SW-L08 | window | window corpus | lean | lean | none | lean |
| SW-L09 | window | window corpus | lean | lean | none | lean |
| SW-L10 | window | window corpus | lean | other | abstention | standard |
| SW-L11 | window | window corpus | lean | lean | none | lean |
| SW-L12 | window | window corpus | lean | other | abstention | standard |
| SW-N01 | new | portless rule | lean | lean | none | lean |
| SW-N02 | new | portless rule | standard | standard | none | standard |
| SW-N03 | new | portless rule | lean | lean | none | lean |
| SW-N04 | new | seam with noise | lean | lean | none | lean |
| SW-N05 | new | seam with noise | standard | standard | none | standard |
| SW-N06 | new | seam with noise | standard | standard | none | standard |
| SW-N07 | new | seam with noise | roomy | roomy | none | roomy |
| SW-N08 | new | trap refresh | lean | lean | none | lean |
| SW-N09 | new | trap refresh | lean | other | abstention | standard |
| SW-N10 | new | enumeration | standard | standard | none | standard |
| SW-N11 | new | enumeration | roomy | roomy | none | roomy |
| SW-N12 | new | undetermined | other | other | abstention | lean |
| SW-R01 | window | window corpus | roomy | roomy | none | roomy |
| SW-R02 | window | window corpus | roomy | roomy | none | roomy |
| SW-R03 | window | window corpus | roomy | roomy | none | roomy |
| SW-R04 | window | window corpus | roomy | roomy | none | roomy |
| SW-R05 | window | window corpus | roomy | roomy | none | roomy |
| SW-R06 | window | window corpus | roomy | roomy | none | roomy |
| SW-R07 | window | window corpus | roomy | other | abstention | lean |
| SW-R08 | window | window corpus | roomy | roomy | none | roomy |
| SW-R09 | window | window corpus | roomy | roomy | none | roomy |
| SW-R10 | window | window corpus | roomy | roomy | none | roomy |
| SW-R11 | window | window corpus | roomy | roomy | none | roomy |
| SW-R12 | window | window corpus | roomy | roomy | none | roomy |
| SW-S01 | window | window corpus | standard | other | abstention | lean |
| SW-S02 | window | window corpus | standard | standard | none | standard |
| SW-S03 | window | window corpus | standard | other | abstention | lean |
| SW-S04 | window | window corpus | standard | standard | none | standard |
| SW-S05 | window | window corpus | standard | standard | none | standard |
| SW-S06 | window | window corpus | standard | other | abstention | lean |
| SW-S07 | window | window corpus | standard | standard | none | standard |
| SW-S08 | window | window corpus | standard | standard | none | standard |
| SW-S09 | window | window corpus | standard | standard | none | standard |
| SW-S10 | window | window corpus | standard | standard | none | standard |
| SW-S11 | window | window corpus | standard | standard | none | standard |
| SW-S12 | window | window corpus | standard | standard | none | standard |
