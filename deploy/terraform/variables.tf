variable "region" {
  description = "AWS region."
  type        = string
  default     = "us-east-1"
}

variable "profile" {
  description = "Shared-credentials profile to use."
  type        = string
  default     = "idealyst"
}

variable "zone" {
  description = "Route 53 hosted zone the hostname lives in. Trailing dot optional."
  type        = string
  default     = "idealyst.io"
}

variable "hostname" {
  description = "Fully qualified name to serve on. Caddy requests a certificate for it."
  type        = string
  default     = "chat.idealyst.io"
}

variable "instance_type" {
  description = <<-EOT
    t3.small is enough to relay. It is NOT enough to run server-side neural
    suppression for several participants at once — that spends real CPU per
    person, which is why `VOICE_DENOISE` is off in the systemd unit. Size up
    before turning it on rather than after.
  EOT
  type        = string
  default     = "t3.small"
}

variable "admin_cidr" {
  description = <<-EOT
    Who may reach SSH. Defaults to the whole internet because the alternative
    is locking yourself out from a laptop with a changing address, and the key
    is the actual control — but narrowing this to your own address is free and
    strictly better if it is stable.
  EOT
  type        = string
  default     = "0.0.0.0/0"
}

variable "volume_gb" {
  description = "Root volume size. Postgres, Redis and the app all live on it."
  type        = number
  default     = 20
}
