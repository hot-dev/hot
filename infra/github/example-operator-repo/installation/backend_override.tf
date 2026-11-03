# Remote state in a bucket you created for it, separate from the installation.
terraform {
  backend "s3" {
    bucket       = "example-hot-tofu-state"
    key          = "hot-example/terraform.tfstate"
    region       = "us-east-1"
    encrypt      = true
    use_lockfile = true
  }
}
