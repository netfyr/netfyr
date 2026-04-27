#!/bin/bash

# COPR build script for netfyr.
#
# This script is meant to be called from a Fedora COPR custom build.
# Add a custom build with:
#
#   #!/bin/bash
#   export GIT_REF=main
#   curl https://raw.githubusercontent.com/bengal/netfyr-test-impl/main/scripts/build-copr.sh | bash
#
# Environment variables:
#   GIT_REF  - git ref to build (branch, tag, or sha). Default: main.

set -ex

if [[ -z "$GIT_REF" ]]; then
    GIT_REF=main
fi

mkdir netfyr
pushd netfyr
git init .
git remote add origin https://github.com/bengal/netfyr-test-impl.git
git fetch origin "$GIT_REF"
git checkout FETCH_HEAD

NAME=$(grep '^Name:' netfyr.spec | awk '{print $2}')
BASE_VERSION=$(sed -n 's/^%global base_version //p' netfyr.spec)
COMMIT=$(git rev-parse --short=7 HEAD)
NUM=$(git rev-list --count HEAD)

sed -i "s/^Version:.*/Version:        ${BASE_VERSION}~dev${NUM}.g${COMMIT}/" netfyr.spec

git archive --format=tar.gz \
    --prefix="${NAME}-${BASE_VERSION}/" \
    -o "${NAME}-${BASE_VERSION}.tar.gz" \
    HEAD

cargo vendor vendor
tar czf "${NAME}-${BASE_VERSION}-vendor.tar.gz" vendor/
rm -rf vendor/

popd

mv netfyr/"${NAME}-${BASE_VERSION}.tar.gz" .
mv netfyr/"${NAME}-${BASE_VERSION}-vendor.tar.gz" .
mv netfyr/netfyr.spec .
rm -rf netfyr
