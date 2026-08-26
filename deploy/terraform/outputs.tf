output "hostname" {
  value = var.hostname
}

output "public_ip" {
  value = aws_eip.chat.public_ip
}

output "ssh" {
  description = "Ready-to-paste SSH command."
  value       = "ssh -i ${path.module}/id_ed25519 admin@${aws_eip.chat.public_ip}"
}

output "url" {
  value = "https://${var.hostname}"
}
