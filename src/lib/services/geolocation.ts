import { GEO_REGIONS, UNKNOWN_REGION_ID, type GeoRegionConfig } from '$lib/geo';

export type GeolocationSource = 'browser' | 'timezone' | 'fallback';

export interface GeolocationResult {
  region: GeoRegionConfig;
  source: GeolocationSource;
}

const REGION_CANDIDATES = GEO_REGIONS.filter((region) => region.id !== UNKNOWN_REGION_ID);

const FALLBACK_REGION = REGION_CANDIDATES.find((region) => region.id === 'usEast') ?? REGION_CANDIDATES[0];

function toRadians(value: number): number {
  return (value * Math.PI) / 180;
}

function haversineDistance(lat1: number, lng1: number, lat2: number, lng2: number): number {
  const R = 6371; // Radius of Earth in km
  const dLat = toRadians(lat2 - lat1);
  const dLng = toRadians(lng2 - lng1);
  const a =
    Math.sin(dLat / 2) * Math.sin(dLat / 2) +
    Math.cos(toRadians(lat1)) *
      Math.cos(toRadians(lat2)) *
      Math.sin(dLng / 2) *
      Math.sin(dLng / 2);
  const c = 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
  return R * c;
}

function nearestRegion(lat: number, lng: number): GeoRegionConfig {
  let closest = FALLBACK_REGION;
  let minDistance = Number.POSITIVE_INFINITY;

  for (const region of REGION_CANDIDATES) {
    const distance = haversineDistance(lat, lng, region.lat, region.lng);
    if (distance < minDistance) {
      minDistance = distance;
      closest = region;
    }
  }

  return closest;
}

function inferRegionFromTimezone(timezone: string): GeoRegionConfig | null {
  const tz = timezone.toLowerCase();

  // Order matters - more specific matches should come before general ones
  const matchers: Array<{ test: (tz: string) => boolean; regionId: string }> = [
    // US West - Pacific states and Mountain states
    {
      test: (value) =>
        /america\/(los_angeles|vancouver|whitehorse|sitka|anchorage|metlakatla|juneau|yakutat|tijuana|phoenix|boise|denver|edmonton|dawson|hermosillo|mazatlan|yellowknife|dawson_creek)/.test(
          value
        ),
      regionId: 'usWest',
    },
    // South America - must come before generic America matcher
    {
      test: (value) =>
        /america\/(argentina|buenos_aires|santiago|sao_paulo|bogota|lima|la_paz|montevideo|caracas|asuncion|guayaquil|fortaleza|bahia|recife|belem|manaus|cuiaba|porto_velho|campo_grande|rio_branco|maceio|cayenne|paramaribo|bogota)/.test(
          value
        ),
      regionId: 'southAmerica',
    },
    // US East and Central - general America catch-all for North American timezones
    {
      test: (value) => /america\//.test(value),
      regionId: 'usEast',
    },
    // EU Central - Central and Eastern European timezones
    {
      test: (value) =>
        /europe\/(berlin|vienna|warsaw|prague|budapest|zurich|amsterdam|brussels|stockholm|oslo|copenhagen|rome|madrid|paris|prague|bucharest|sofia|athens|helsinki|tallinn|riga|vilnius|belgrade|zagreb|ljubljana|sarajevo|skopje|kiev|moscow|minsk)/.test(
          value
        ),
      regionId: 'euCentral',
    },
    // EU West - UK, Ireland, Portugal
    {
      test: (value) =>
        /europe\/(london|dublin|lisbon|reykjavik|isle_of_man|jersey|guernsey)/.test(value),
      regionId: 'euWest',
    },
    // EU catch-all (default to euCentral for unmatched European timezones)
    {
      test: (value) => /europe\//.test(value),
      regionId: 'euCentral',
    },
    // Asia East - Japan, Korea, China
    {
      test: (value) =>
        /asia\/(tokyo|seoul|shanghai|chongqing|hong_kong|taipei|macau|harbin|urumqi|pyongyang)/.test(
          value
        ),
      regionId: 'asiaEast',
    },
    // Asia Pacific - Southeast Asia, India
    {
      test: (value) =>
        /asia\/(singapore|kuala_lumpur|jakarta|manila|ho_chi_minh|bangkok|yangon|phnom_penh|vientiane|calcutta|kolkata|mumbai|delhi|bangalore|chennai|kathmandu|dhaka|colombo|karachi|kabul|tehran|dubai|riyadh|bahrain|qatar|muscat|kuwait)/.test(
          value
        ),
      regionId: 'asiaPacific',
    },
    // Indian Ocean timezones default to Asia Pacific
    {
      test: (value) => /indian\//.test(value),
      regionId: 'asiaPacific',
    },
    // Asia catch-all
    {
      test: (value) => /asia\//.test(value),
      regionId: 'asiaPacific',
    },
    // Oceania - Australia and Pacific
    {
      test: (value) =>
        /(australia|pacific)\/(sydney|melbourne|brisbane|perth|adelaide|hobart|darwin|auckland|wellington|fiji|guam|port_moresby|noumea|tahiti|honolulu)/.test(
          value
        ),
      regionId: 'oceania',
    },
    // Oceania catch-all
    {
      test: (value) => /(australia|pacific)\//.test(value),
      regionId: 'oceania',
    },
    // Africa
    {
      test: (value) =>
        /africa\/(cairo|johannesburg|lagos|nairobi|casablanca|algiers|tunis|accra|addis_ababa|khartoum|dar_es_salaam|kampala|harare|lusaka|maputo|kinshasa|luanda|windhoek|gaborone|cape_town)/.test(
          value
        ),
      regionId: 'africa',
    },
    // Africa catch-all
    {
      test: (value) => /africa\//.test(value),
      regionId: 'africa',
    },
    // Atlantic timezones - map to closest region
    {
      test: (value) => /atlantic\/azores/.test(value),
      regionId: 'euWest',
    },
    {
      test: (value) => /atlantic\/bermuda/.test(value),
      regionId: 'usEast',
    },
    {
      test: (value) => /atlantic\//.test(value),
      regionId: 'euWest',
    },
  ];

  for (const matcher of matchers) {
    if (matcher.test(tz)) {
      const region = REGION_CANDIDATES.find((item) => item.id === matcher.regionId);
      if (region) {
        return region;
      }
    }
  }

  return null;
}

async function detectFromBrowserGeolocation(): Promise<GeoRegionConfig | null> {
  if (typeof window === 'undefined' || !('geolocation' in navigator)) {
    return null;
  }

  try {
    const position = await new Promise<GeolocationPosition>((resolve, reject) => {
      navigator.geolocation.getCurrentPosition(resolve, reject, {
        enableHighAccuracy: false,
        maximumAge: 60_000,
        timeout: 5_000,
      });
    });

    const { latitude, longitude } = position.coords;
    return nearestRegion(latitude, longitude);
  } catch (error) {
    return null;
  }
}

function detectFromTimezone(): GeoRegionConfig | null {
  if (typeof Intl === 'undefined' || typeof Intl.DateTimeFormat === 'undefined') {
    return null;
  }

  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  if (!timezone) {
    return null;
  }

  return inferRegionFromTimezone(timezone);
}

export async function detectUserRegion(): Promise<GeolocationResult> {
  const browserRegion = await detectFromBrowserGeolocation();
  if (browserRegion) {
    return { region: browserRegion, source: 'browser' };
  }

  const timezoneRegion = detectFromTimezone();
  if (timezoneRegion) {
    return { region: timezoneRegion, source: 'timezone' };
  }

  return { region: FALLBACK_REGION, source: 'fallback' };
}

export function calculateRegionDistance(a: GeoRegionConfig, b: GeoRegionConfig): number {
  return haversineDistance(a.lat, a.lng, b.lat, b.lng);
}