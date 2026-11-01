PORTNAME=	wdpmcd
DISTVERSIONPREFIX=	v
DISTVERSION=	0.3.0
CATEGORIES=	sysutils

MAINTAINER=	3768228+wakarei@users.noreply.github.com
COMMENT=	Fan and LED control daemon for the WD My Cloud PR2100/PR4100
WWW=		https://github.com/wakarei/wdpmcd

LICENSE=	BSD2CLAUSE
LICENSE_FILE=	${WRKSRC}/LICENSE

ONLY_FOR_ARCHS=	amd64
ONLY_FOR_ARCHS_REASON=	the WD My Cloud PMC is only found on x86-64 hardware

USES=		cargo
USE_GITHUB=	yes
GH_ACCOUNT=	wakarei

CARGO_CRATES=	bitflags-2.13.2 \
		cfg-if-1.0.5 \
		cfg_aliases-0.2.2 \
		errno-0.3.14 \
		libc-0.2.191 \
		nix-0.31.3 \
		signal-hook-0.4.5 \
		signal-hook-registry-1.4.8 \
		windows-link-0.2.1 \
		windows-sys-0.61.2

post-install:
	${MKDIR} ${STAGEDIR}${PREFIX}/sbin
	${MV} ${STAGEDIR}${PREFIX}/bin/wdpmcd ${STAGEDIR}${PREFIX}/sbin/wdpmcd
	${INSTALL_SCRIPT} ${WRKSRC}/etc/rc.d/wdpmcd ${STAGEDIR}${PREFIX}/etc/rc.d/wdpmcd
	${INSTALL_DATA} ${WRKSRC}/etc/wdpmcd.conf.sample ${STAGEDIR}${PREFIX}/etc/wdpmcd.conf.sample
	${INSTALL_MAN} ${WRKSRC}/man/wdpmcd.8 ${STAGEDIR}${PREFIX}/share/man/man8
	${INSTALL_MAN} ${WRKSRC}/man/wdpmcd.conf.5 ${STAGEDIR}${PREFIX}/share/man/man5

.include <bsd.port.mk>
