#!/bin/bash
set -euo pipefail

SPEC="netfyr.spec"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

# Extract Name and Version from the spec file (resolve RPM macros first)
PARSED=$(rpmspec --parse "$SPEC" 2>/dev/null)
NAME=$(echo "$PARSED" | grep '^Name:' | awk '{print $2}')
VERSION=$(echo "$PARSED" | grep '^Version:' | awk '{print $2}')

# Create rpmbuild directory structure
mkdir -p ~/rpmbuild/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

# Create source tarball from git
echo "Creating source tarball ${NAME}-${VERSION}.tar.gz ..."
git archive --format=tar.gz \
    --prefix="${NAME}-${VERSION}/" \
    -o ~/rpmbuild/SOURCES/"${NAME}-${VERSION}.tar.gz" \
    HEAD

# Create vendor tarball (%%cargo_prep forces offline mode, so dependencies
# must be vendored — but vendor/ is gitignored and never committed)
echo "Creating vendor tarball ${NAME}-${VERSION}-vendor.tar.gz ..."
cargo vendor vendor
tar czf ~/rpmbuild/SOURCES/"${NAME}-${VERSION}-vendor.tar.gz" vendor/
rm -rf vendor/

# Copy the spec file
cp "$SPEC" ~/rpmbuild/SPECS/

# Build the RPM
echo "Building RPM ..."
rpmbuild -ba ~/rpmbuild/SPECS/"$SPEC"

echo ""
echo "Build complete. RPMs:"
find ~/rpmbuild/RPMS/ -name "${NAME}*.rpm" -type f 2>/dev/null
find ~/rpmbuild/SRPMS/ -name "${NAME}*.src.rpm" -type f 2>/dev/null
