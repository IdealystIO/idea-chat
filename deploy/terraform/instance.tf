# An SSH key generated here rather than uploaded, so a fresh clone of this repo
# can stand the whole thing up without hunting for a `.pem` somebody made once.
# The private key is written to disk with 0600 and is gitignored.
resource "tls_private_key" "ssh" {
  algorithm = "ED25519"
}

resource "aws_key_pair" "ssh" {
  key_name   = "chat-idealyst-io"
  public_key = tls_private_key.ssh.public_key_openssh
}

resource "local_sensitive_file" "ssh" {
  content         = tls_private_key.ssh.private_key_openssh
  filename        = "${path.module}/id_ed25519"
  file_permission = "0600"
}

resource "aws_security_group" "chat" {
  name        = "chat-idealyst-io"
  description = "Hearth: web, plus the UDP port the voice node is dialled on"
  vpc_id      = data.aws_vpc.default.id

  ingress {
    description = "SSH"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.admin_cidr]
  }

  ingress {
    description = "HTTP: Caddy redirects to HTTPS and answers ACME challenges"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "HTTPS: the app, the API, and the voice WebSocket"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  # The one rule worth reading twice. A QUIC session cannot be reverse-proxied,
  # so the voice node is dialled DIRECTLY on this port — not through Caddy, not
  # through anything. Close it and every call still works, over the WebSocket,
  # with nothing in the UI to say the transport changed.
  ingress {
    description = "WebTransport/QUIC: the voice node, dialled directly (not proxyable)"
    from_port   = 8798
    to_port     = 8798
    protocol    = "udp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { Name = "chat-idealyst-io" }
}

resource "aws_instance" "chat" {
  ami                    = data.aws_ami.debian.id
  instance_type          = var.instance_type
  key_name               = aws_key_pair.ssh.key_name
  vpc_security_group_ids = [aws_security_group.chat.id]
  subnet_id              = data.aws_subnets.default.ids[0]

  root_block_device {
    volume_size = var.volume_gb
    volume_type = "gp3"
    encrypted   = true
  }

  # Everything the box needs that is not the app itself. The app arrives later,
  # by `ship.sh`, because it is built elsewhere.
  user_data = templatefile("${path.module}/bootstrap.sh.tftpl", {
    hostname = var.hostname
  })

  # Changing user_data would otherwise replace the instance and take the
  # database with it. Bootstrap runs once; anything after it is `ship.sh`.
  user_data_replace_on_change = false

  tags = { Name = "chat-idealyst-io" }
}

# So the address survives a stop/start, and so the DNS record does not have to
# be chased. Free while attached to a running instance.
resource "aws_eip" "chat" {
  instance = aws_instance.chat.id
  domain   = "vpc"
  tags     = { Name = "chat-idealyst-io" }
}

resource "aws_route53_record" "chat" {
  zone_id = data.aws_route53_zone.zone.zone_id
  name    = var.hostname
  type    = "A"
  ttl     = 60
  records = [aws_eip.chat.public_ip]
}
