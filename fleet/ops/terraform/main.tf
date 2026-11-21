# Run through scripts/fcvm-fleet-ops on fcvm-arm. The backend bucket and the
# short-lived member-account credentials come from /infra/fcvm-fleet.
terraform {
  required_version = "= 1.10.3"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "= 6.64.0"
    }
  }

  backend "s3" {
    region               = "us-east-2"
    key                  = "fleet-ops/terraform.tfstate"
    workspace_key_prefix = "fleet-ops/workspaces"
    use_lockfile         = true
    encrypt              = true
  }
}

variable "fleet_account_id" {
  type = string
  validation {
    condition     = can(regex("^[0-9]{12}$", var.fleet_account_id))
    error_message = "Use the fleet member account from /infra/fcvm-fleet."
  }
}

provider "aws" {
  region              = "us-east-2"
  allowed_account_ids = [var.fleet_account_id]
}

resource "aws_cloudwatch_log_group" "operations" {
  name              = "/fcvm/ops/deployments"
  retention_in_days = 30
}

resource "aws_ssm_parameter" "deployment_log_group" {
  name  = "/fcvm/ops/deployment-log-group"
  type  = "String"
  value = aws_cloudwatch_log_group.operations.name
}
