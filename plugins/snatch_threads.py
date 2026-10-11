"""yt-dlp extractor plugin for Threads posts (threads.com / threads.net), shipped by SNATCH.

yt-dlp has no Threads extractor, and the post page is a client-rendered shell: the
media comes from the web client's own GraphQL query (BarcelonaPostPageTargetQuery),
which works for public posts without logging in. The query is a *persisted* query,
addressed by an id that changes whenever Meta recompiles it, so the id is found on
demand (and cached) instead of being hard-wired:

1. the id built into this file is tried first;
2. if Threads rejects it, the page's own JavaScript bundles are searched for the
   current id and the list of variables the query now requires, and the answer is
   cached for later runs.

SNATCH writes this file into its own plugin folder and points yt-dlp at it
(--plugin-dirs); it is not meant to be edited in place, the app overwrites it.
"""
import os
import re
import urllib.request
import xml.etree.ElementTree as ET

from yt_dlp.extractor.common import InfoExtractor
from yt_dlp.utils import (
    ExtractorError,
    clean_html,
    int_or_none,
    join_nonempty,
    traverse_obj,
    url_or_none,
    urlencode_postdata,
)

_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_'
_APP_ID = '238260118697367'
_GRAPHQL_URL = 'https://www.threads.com/api/graphql'
_QUERY_NAME = 'BarcelonaPostPageTargetQuery'
_BROWSER_UA = (
    'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) '
    'Chrome/130.0.0.0 Safari/537.36')

# The query id and the "provided" variables of the build this plugin was written against.
_BUILT_IN_DOC_ID = '28881847464743900'
_BUILT_IN_FLAGS = (
    'BarcelonaCanSeeSponsoredContent', 'BarcelonaGenAIRepliesEnabled', 'BarcelonaHasBestOfThreads',
    'BarcelonaHasCommunities', 'BarcelonaHasCommunityEmojiUpdateCard', 'BarcelonaHasCommunityEntityCard',
    'BarcelonaHasCommunityPermalinkPivots', 'BarcelonaHasCommunityTopContributors',
    'BarcelonaHasCustomLikesConsumption', 'BarcelonaHasDearAlgoConsumption',
    'BarcelonaHasDearAlgoWebProduction', 'BarcelonaHasEventBadge', 'BarcelonaHasGameScoreShare',
    'BarcelonaHasGhostPostEmojiActivation', 'BarcelonaHasGhostPostProduction',
    'BarcelonaHasInsightsPermalinkUFI', 'BarcelonaHasMessaging', 'BarcelonaHasMetaAiContentAttachments',
    'BarcelonaHasNewsletterLinkPreview', 'BarcelonaHasNewspaperLinkStyle',
    'BarcelonaHasPodcastTranscriptConsumption', 'BarcelonaHasPodcastV2Consumption',
    'BarcelonaHasPodcastV2Production', 'BarcelonaHasPrivateRepliesDeprecation',
    'BarcelonaHasPublicViewCountCard', 'BarcelonaHasScorecardCommunity',
    'BarcelonaHasSportTeamAllegianceCard', 'BarcelonaHasUnseenReplies', 'BarcelonaHasViewerReplied',
    'BarcelonaHasWebFavicons', 'BarcelonaIsCrawler', 'BarcelonaIsInternalUser', 'BarcelonaIsLoggedIn',
    'BarcelonaIsSearchDiscoveryEnabled', 'BarcelonaMessagesHasLiveChatMessaging',
    'BarcelonaMessagingHasMetaAIBot', 'BarcelonaOptionalCookiesEnabled',
    'BarcelonaShouldFetchPostAuthorFullName', 'BarcelonaShouldFulfillLightboxQuery',
    'BarcelonaShouldShowFediverseM075Features', 'BarcelonaShouldShowFediverseM1Features',
)

# Upper bound on how many bundles one lookup downloads before giving up.
_MAX_BUNDLES = 400
_MAX_SHELL_BYTES = 8 * 1024 * 1024  # the app shell is a few hundred KB; bound what a host may send


def _flag_variable(name):
    return f'__relay_internal__pv__{name}relayprovider'


class SnatchThreadsIE(InfoExtractor):
    IE_NAME = 'snatch:threads'
    IE_DESC = 'Threads posts (SNATCH plugin)'
    _VALID_URL = (
        r'https?://(?:www\.)?threads\.(?:com|net)/'
        r'(?:(?:@[^/?#]+/)?post/(?P<id>[\w-]+)|share/(?P<share>[\w-]+))'
    )
    _CACHE_SECTION = 'snatch-threads'

    # -- helpers ---------------------------------------------------------------------

    @staticmethod
    def _code_to_pk(code):
        """The numeric media id behind a post's short code (Instagram's base-64 alphabet)."""
        pk = 0
        for char in code:
            index = _ALPHABET.find(char)
            if index < 0:
                raise ExtractorError(f'"{code}" is not a Threads post code', expected=True)
            pk = pk * 64 + index
        return str(pk)

    def _request_data(self, page_url, code, lsd, post_id, query):
        variables = {'postID': post_id}
        variables.update({_flag_variable(name): False for name in query['flags']})
        return self._download_json(
            _GRAPHQL_URL, code, 'Downloading post data', fatal=False,
            data=urlencode_postdata({
                'lsd': lsd,
                'variables': self._json_dumps(variables),
                'doc_id': query['doc_id'],
                'fb_api_req_friendly_name': _QUERY_NAME,
                'server_timestamps': 'true',
            }),
            headers={
                'Content-Type': 'application/x-www-form-urlencoded',
                'X-FB-LSD': lsd,
                'X-IG-App-ID': _APP_ID,
                'X-FB-Friendly-Name': _QUERY_NAME,
                'Origin': 'https://www.threads.com',
                'Referer': page_url,
                'Sec-Fetch-Site': 'same-origin',
                'Sec-Fetch-Mode': 'cors',
                'Sec-Fetch-Dest': 'empty',
            })

    @staticmethod
    def _json_dumps(value):
        import json
        return json.dumps(value, separators=(',', ':'))

    def _known_query(self):
        override = os.environ.get('SNATCH_THREADS_DOC_ID')
        if override:  # diagnostics: forces the lookup path
            return {'doc_id': override, 'flags': list(_BUILT_IN_FLAGS)}
        cached = self.cache.load(self._CACHE_SECTION, 'query')
        if isinstance(cached, dict) and cached.get('doc_id') and cached.get('flags'):
            return cached
        return {'doc_id': _BUILT_IN_DOC_ID, 'flags': list(_BUILT_IN_FLAGS)}

    def _discover_query(self, page_url, webpage, code):
        """Find the current query id and its variables in the page's JavaScript bundles."""
        # The app shell differs by user agent (some get a crawler-oriented page whose script
        # list lacks the post-page bundle), so ask for the shell a desktop browser gets.
        # yt-dlp's own request headers (Sec-Fetch-Mode: navigate, Accept-Language) make Threads
        # serve the full server-rendered variant instead, so this one request is made with
        # the bare minimum a browser's first visit sends. The standard library honours the
        # system proxy settings just like yt-dlp does.
        try:
            request = urllib.request.Request(page_url, headers={'User-Agent': _BROWSER_UA})
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read(_MAX_SHELL_BYTES + 1)
            if len(raw) > _MAX_SHELL_BYTES:
                raise ValueError('the page is larger than expected')
            shell = raw.decode('utf-8', 'replace')
        except Exception as error:
            self.write_debug(f'Could not fetch the app shell: {error}')
            shell = webpage
        sources = list(dict.fromkeys(re.findall(
            r'https://static\.cdninstagram\.com/rsrc\.php/[^"\\\s]+?\.js', shell.replace('\\/', '/'))))
        self.write_debug(f'app shell: {len(shell)} bytes, {len(sources)} scripts')
        self.to_screen(
            f'Threads rejected the built-in query id; looking up the current one in {len(sources)} '
            'page scripts (done once, the answer is cached)')
        doc_id = flags = None
        for source in sources[:_MAX_BUNDLES]:
            script = self._download_webpage(source, code, note=False, errnote=False, fatal=False)
            if not script or _QUERY_NAME not in script:
                continue
            if doc_id is None:
                match = re.search(
                    rf'__d\("{_QUERY_NAME}_threadsRelayOperation",\[\],\(function\([^)]*\)\{{[^}}]*?exports="(\d+)"',
                    script)
                doc_id = match.group(1) if match else None
            if flags is None:
                start = script.find(f'__d("{_QUERY_NAME}$Parameters.threads"')
                if start >= 0:
                    end = script.find('__d(', start + 10)
                    body = script[start:end if end > 0 else None]
                    names = re.findall(r'__relay_internal__pv__(\w+?)relayprovider', body)
                    flags = list(dict.fromkeys(names)) or None
            if doc_id and flags:
                break
        if not doc_id:
            return None
        query = {'doc_id': doc_id, 'flags': flags or list(_BUILT_IN_FLAGS)}
        self.cache.store(self._CACHE_SECTION, 'query', query)
        return query

    def _fetch_media(self, page_url, code, post_id, lsd, webpage):
        query = self._known_query()
        for attempt in range(2):
            data = self._request_data(page_url, code, lsd, post_id, query)
            if isinstance(data, dict) and 'media' in (data.get('data') or {}):
                media = data['data']['media']
                if media:
                    return media
                raise ExtractorError(
                    'This Threads post is not available (deleted, or it needs a login)', expected=True)
            if attempt == 0:
                query = self._discover_query(page_url, webpage, code)
                if query:
                    continue
            break
        raise ExtractorError(
            'Threads did not return this post and the query id could not be refreshed; '
            'the site has probably changed (update SNATCH)', expected=True)

    # -- extraction ------------------------------------------------------------------

    def _mpd_formats(self, item, item_id):
        manifest = item.get('video_dash_manifest')
        if not manifest:
            return []
        try:
            document = ET.fromstring(manifest)
        except ET.ParseError:
            return []
        try:
            return self._parse_mpd_formats(document, mpd_id='dash', mpd_base_url='', mpd_url=None) or []
        except Exception as error:  # a broken manifest must not hide the progressive file
            self.report_warning(f'Could not read the DASH manifest of {item_id}: {error}')
            return []

    def _item_formats(self, item, item_id):
        formats = []
        seen = set()
        no_audio = item.get('has_audio') is False
        for version in item.get('video_versions') or []:
            url = url_or_none(version.get('url'))
            # The "types" are CDN variants of one and the same file.
            if not url or url.split('?')[0] in seen:
                continue
            seen.add(url.split('?')[0])
            formats.append({
                'url': url,
                'format_id': f'progressive-{version.get("type") or len(formats)}',
                'ext': 'mp4',
                'vcodec': 'h264',
                'acodec': 'none' if no_audio else 'aac',
                'http_headers': {'Referer': 'https://www.threads.com/'},
            })
        formats.extend(self._mpd_formats(item, item_id))
        return formats

    def _real_extract(self, url):
        match = self._match_valid_url(url)
        page_url = url
        if match.group('share'):
            response = self._request_webpage(url, match.group('share'), 'Resolving the share link')
            page_url = response.url
            match = self._match_valid_url(page_url)
            if not (match and match.group('id')):
                raise ExtractorError('The share link does not lead to a Threads post', expected=True)
        code = match.group('id')
        post_id = self._code_to_pk(code)

        webpage = self._download_webpage(page_url, code, 'Downloading the post page')
        lsd = self._search_regex(r'"LSD",\[\],\{"token":"([^"]+)"', webpage, 'LSD token')
        media = self._fetch_media(page_url, code, post_id, lsd, webpage)

        author = media.get('user') or {}
        caption = clean_html(traverse_obj(media, ('caption', 'text'))) or ''
        title = (caption.split('\n')[0].strip()[:80] if caption else None) or join_nonempty(
            author.get('username'), code, delim=' - ')
        common = {
            'uploader': author.get('full_name') or author.get('username'),
            'uploader_id': author.get('username'),
            'timestamp': int_or_none(media.get('taken_at')),
            'like_count': int_or_none(media.get('like_count')),
            'description': caption or None,
            'webpage_url': page_url,
        }

        items = [item for item in (media.get('carousel_media') or [media])
                 if item.get('video_versions') or item.get('video_dash_manifest')]
        if not items:
            raise ExtractorError('This Threads post has no video', expected=True)

        entries = []
        for index, item in enumerate(items, 1):
            item_id = code if len(items) == 1 else f'{code}_{index}'
            formats = self._item_formats(item, item_id)
            if not formats:
                continue
            entries.append({
                **common,
                'id': item_id,
                'title': title if len(items) == 1 else f'{title} ({index})',
                'formats': formats,
                'thumbnails': traverse_obj(item, ('image_versions2', 'candidates', ..., {
                    'url': ('url', {url_or_none}),
                    'width': ('width', {int_or_none}),
                    'height': ('height', {int_or_none}),
                })) or None,
            })
        if not entries:
            raise ExtractorError('This Threads post has no playable video', expected=True)
        if len(entries) == 1:
            return entries[0]
        return self.playlist_result(entries, code, title, caption or None)
