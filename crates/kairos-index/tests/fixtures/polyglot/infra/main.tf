provider "aws" {
  region = var.region
}

locals {
  name = "ledger-${var.region}"
}

data "aws_caller_identity" "current" {}

module "store" {
  source = "./modules/bucket"
  name   = local.name
  owner  = data.aws_caller_identity.current.account_id
}

module "network" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.0.0"
  name    = local.name
}

resource "aws_sns_topic" "events" {
  name = "${local.name}-events"
}

output "bucket_arn" {
  value = module.store.arn
}
