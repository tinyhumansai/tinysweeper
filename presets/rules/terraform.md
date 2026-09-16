## Terraform

### Report

- A `required_providers` or module `source` with no version constraint at
  all, where sibling entries in the same file do pin one — a build that is
  reproducible everywhere except here.
- A literal secret — a password, API key, access key pair, private key, or
  connection string — assigned directly to a resource argument or a
  `variable`/`locals` default, instead of coming from a secret manager or a
  `sensitive` input.
- An `output` or a non-`sensitive` `variable` that exposes a value the
  provider itself marks sensitive (a generated password, a private key
  attribute).
- A security group, firewall rule, or network ACL with an unrestricted source
  (`0.0.0.0/0`, `::/0`) on a sensitive port — SSH, RDP, a database port — or
  on all ports.
- An IAM policy or resource policy granting a wildcard action (`"Action":
  "*"`) or wildcard resource (`"Resource": "*"`) where a scoped set would
  cover the actual usage shown in the diff.
- Removing or weakening a `lifecycle { prevent_destroy = true }` guard on a
  stateful resource — database, persistent volume, KMS key — with no
  explanation in the diff.
- A `.tfstate` or `.tfstate.backup` file included in the diff. State can
  contain every attribute of every resource, secrets included, in plaintext.

### Do NOT report

- A deliberately wide version constraint (`~>`, an explicit range) that
  matches the pattern of sibling entries in the same file.
- A resource with no `lifecycle` block where nothing else in the diff or the
  file's sibling resources of the same kind has one either — consistency is
  the finding, not the absence on its own.
- A public read setting on a storage resource whose name, tags, or
  surrounding configuration make clear it serves public content (a CDN
  origin, a static site bucket).
- Runtime provider behavior, cloud account configuration, or state stored
  outside the files in the diff — nothing here can confirm what actually
  exists in the account.
- Formatting or attribute ordering that `terraform fmt` would silently fix.
- A `sensitive = true` variable still visible in a `terraform plan` diff
  output shown elsewhere — that visibility is Terraform's, not this file's,
  to fix.
