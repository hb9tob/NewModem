DOCKER ?= docker
APPIMAGE_DIST ?= dist/appimage

.PHONY: appimage-docker
appimage-docker:
	DOCKER_BUILDKIT=1 $(DOCKER) build \
		--platform linux/amd64 \
		--file docker/appimage.Dockerfile \
		--target artifact \
		--output "type=local,dest=$(APPIMAGE_DIST)" \
		.
	@printf 'AppImage: %s/nbfm-modem-gui-x86_64.AppImage\n' "$(APPIMAGE_DIST)"