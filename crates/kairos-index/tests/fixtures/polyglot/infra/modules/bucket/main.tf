resource "aws_s3_bucket" "this" {
  count  = 1
  bucket = var.name
  tags   = { owner = var.owner }
}

resource "aws_sns_topic" "events" {
  for_each = toset(["created"])
  name     = "${var.name}-${each.key}"
}
